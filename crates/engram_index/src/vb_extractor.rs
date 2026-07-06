use crate::parsing::{ExtractedEdge, ExtractedSymbol};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::Duration;

// SQL detection for the fallback extractor. The Roslyn sidecar emits richer
// SQL edges, but when it is unavailable the fallback must still keep the
// DB half of the graph wired — losing sql_calls edges silently disconnects
// stored procedures from their VB callers.
static RE_VB_SQL_CMD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)New\s+SqlCommand\s*\(\s*"([^"]*)""#).expect("valid VB SqlCommand regex")
});
static RE_VB_COMMAND_TEXT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)CommandText\s*=\s*"([^"]*)""#).expect("valid VB CommandText regex")
});
// TODO-17: dynamic SQL — the command text is a VARIABLE, not a literal.
// These were silently dropped; now they emit a marked sql_calls edge so
// reports can say "plus N dynamic SQL commands the graph can't parse".
static RE_VB_SQL_CMD_DYN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)New\s+SqlCommand\s*\(\s*([A-Za-z_]\w*)\s*[,)]"#)
        .expect("valid VB dynamic SqlCommand regex")
});
static RE_VB_COMMAND_TEXT_DYN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)CommandText\s*=\s*([A-Za-z_]\w*)\s*$"#)
        .expect("valid VB dynamic CommandText regex")
});
// Qualified call sites (Foo.Bar(...)). The optional "New" capture lets us
// skip constructor invocations without lookbehind support.
static RE_VB_QUALIFIED_CALL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(New\s+)?([A-Za-z_]\w*(?:\.[A-Za-z_]\w*)+)\s*\(")
        .expect("valid VB qualified call regex")
});
/// Heads of qualified names that are framework/state noise, not project calls.
const VB_CALL_HEAD_STOPWORDS: [&str; 10] = [
    "me", "my", "mybase", "string", "convert", "integer", "double", "math", "response", "request",
];
// Designer-style control fields: `Protected WithEvents btnSave As ...Button`.
// Parity with the C# tree-sitter pass, which maps designer fields to
// control_ref symbols so they merge with the page's control nodes.
static RE_VB_WITHEVENTS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bWithEvents\s+([A-Za-z_]\w*)\s+As\b").expect("valid VB WithEvents regex")
});
// Settings reads: ConfigurationManager.AppSettings("Key") (VB call syntax)
// and My.Settings.Key. Generic name shapes only.
static RE_VB_APPSETTINGS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)AppSettings\s*\(\s*"([^"]+)"\s*\)"#).expect("valid VB appsettings regex")
});
static RE_VB_MY_SETTINGS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bMy\.Settings\.([A-Za-z_]\w*)").expect("valid My.Settings regex")
});
// Settings-STORE property reads: the dominant house pattern in mature apps
// is a static store class (ConfigSettings.Multitenant.IsMaster,
// SystemSettingStore.General.RoqEnableListTypeDimension) — NOT raw
// AppSettings("...") calls. Without this shape, settings intelligence
// (list_settings / derive_test_matrix) sees zero settings on exactly the
// codebases that need it most. Generic: root identifier must carry a
// settings/config token; 1-3 dotted property segments; excludes the
// ConfigurationManager/WebConfigurationManager framework roots (already
// covered by the AppSettings shape above).
static RE_VB_SETTINGS_STORE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b((?:[A-Za-z_]\w*)?(?:Setting|Config|UserAccess|Permission)\w*)\.((?:[A-Za-z_]\w*\.){0,2}[A-Za-z_]\w*)\b",
    )
    .expect("valid VB settings-store regex")
});
// Permission checks by call-name shape (IsInRole, IsUserInRole, IsXxxAdmin,
// CheckAccessLevel, HasPermission, RequireRole, DemandAdmin, Authorize...).
static RE_VB_GUARD_CALL: LazyLock<Regex> = LazyLock::new(|| {
    // `check(read|write)` = the CheckRead/CheckWrite(permissionObject)
    // idiom, and `check_<entity>id` = the Check_pr_id/Check_rv_id
    // project-access-scoping family — both are the DOMINANT guard style
    // in DAO-delegated codebases, and both were invisible to the old
    // pattern (it demanded a literal access/permission/role substring,
    // so guard-map tools reported provably-guarded methods as UNGUARDED
    // — knowledge-pack harvest 2026-07-06, corroborated across domains).
    Regex::new(
        r"(?i)\b(is[a-z0-9_]*admin[a-z0-9_]*|isinrole|isuserinrole|is[a-z0-9_]*role|check[a-z0-9_]*(access|permission|role)[a-z0-9_]*|check(?:read|write)[a-z0-9_]*|check_[a-z0-9_]*id|has[a-z0-9_]*(permission|access|role)[a-z0-9_]*|require[a-z0-9_]*(role|permission|admin)[a-z0-9_]*|demand[a-z0-9_]*|authorize[a-z0-9_]*)\s*\(",
    )
    .expect("valid VB guard regex")
});
static RE_VB_GUARD_ROLE_LITERAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\b(?:isinrole|isuserinrole)\s*\(\s*"([^"]+)""#)
        .expect("valid VB role literal regex")
});
// LINQ-to-SQL / EF context variables: `Dim db As New iFaltDataContext` /
// `Using db As New FooDbContext` / `db = New BarDataContext`. The ORM DAL
// idiom is otherwise completely invisible to SQL-literal extraction.
static RE_VB_CTX_DECL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:Dim|Using)\s+(\w+)\s+As\s+New\s+\w*D(?:ata|b)Context\b")
        .expect("valid VB ctx decl regex")
});
static RE_VB_CTX_ASSIGN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(\w+)\s*=\s*New\s+\w*D(?:ata|b)Context\b")
        .expect("valid VB ctx assign regex")
});
/// `ctx.TableProp` member access. Method calls (followed by `(`) are
/// filtered by the caller — the regex crate has no lookahead.
static RE_VB_CTX_MEMBER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(\w+)\.([A-Za-z_]\w*)").expect("valid ctx member regex"));
/// Write calls: `ctx.Table.InsertOnSubmit(x)` etc.
static RE_VB_CTX_WRITE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(\w+)\.(\w+)\.(?:InsertOnSubmit|InsertAllOnSubmit|DeleteOnSubmit|DeleteAllOnSubmit|Add|AddRange|Remove|RemoveRange)\s*\(")
        .expect("valid ctx write regex")
});
/// Context members that are ORM machinery, not tables.
const CTX_NON_TABLE_MEMBERS: [&str; 12] = [
    "connection",
    "transaction",
    "log",
    "commandtimeout",
    "deferredloadingenabled",
    "loadoptions",
    "objecttrackingenabled",
    "mapping",
    "database",
    "changetracker",
    "configuration",
    "entry",
];

struct Sidecar {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Option<BufReader<ChildStdout>>,
}

static SIDECAR: OnceLock<Mutex<Option<Sidecar>>> = OnceLock::new();

fn sidecar_binary_path() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("engram_server"));
    let dir = exe.parent().unwrap_or_else(|| Path::new("."));
    let name = match std::env::consts::OS {
        "windows" => "vb_roslyn_sidecar-win-x64.exe",
        "macos" => "vb_roslyn_sidecar-osx-x64",
        _ => "vb_roslyn_sidecar-linux-x64",
    };
    dir.join(name)
}

fn get_or_spawn_sidecar() -> &'static Mutex<Option<Sidecar>> {
    SIDECAR.get_or_init(|| Mutex::new(None))
}

fn ensure_sidecar(guard: &mut Option<Sidecar>) -> std::io::Result<&mut Sidecar> {
    if guard.is_none() {
        let sidecar_bin = sidecar_binary_path();
        let mut child = Command::new(sidecar_bin)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        if let Some(stderr) = child.stderr.take() {
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stderr);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) => break,
                        Ok(_) => tracing::warn!(message = line.trim_end(), "vb_sidecar_stderr"),
                        Err(err) => {
                            tracing::warn!("vb_sidecar_stderr read failed: {err}");
                            break;
                        }
                    }
                }
            });
        }

        let stdin = child.stdin.take().expect("sidecar stdin should be piped");
        let stdout = BufReader::new(child.stdout.take().expect("sidecar stdout should be piped"));
        *guard = Some(Sidecar {
            child,
            stdin: Some(stdin),
            stdout: Some(stdout),
        });
    }
    Ok(guard.as_mut().expect("sidecar initialized"))
}

#[derive(Debug)]
enum SidecarParseError {
    Protocol(anyhow::Error),
    Sidecar(String),
}

#[derive(Serialize)]
struct SidecarRequest<'a> {
    cmd: &'a str,
    path: String,
    source: &'a str,
}

#[derive(Debug, Deserialize)]
struct SidecarResponse {
    #[serde(default)]
    symbols: Vec<SidecarSymbol>,
    #[serde(default)]
    edges: Vec<SidecarEdge>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SidecarSymbol {
    name: String,
    kind: String,
    start_line: u32,
    end_line: u32,
    #[serde(default)]
    metadata: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct SidecarEdge {
    source_name: String,
    source_kind: String,
    source_start_line: u32,
    source_language: String,
    target_name: String,
    target_kind: Option<String>,
    target_start_line: Option<u32>,
    kind: String,
    #[serde(default)]
    metadata: HashMap<String, String>,
}

fn parse_via_sidecar(
    sidecar: &mut Sidecar,
    path: &Path,
    source: &str,
) -> Result<(Vec<ExtractedSymbol>, Vec<ExtractedEdge>), SidecarParseError> {
    // WEDGE-2026-06-12: the request write used to be a plain blocking
    // writeln! into the child's stdin pipe. When the child dies mid-exchange
    // (and an inherited handle keeps the pipe's read end alive — a classic
    // Windows hazard), the write blocks FOREVER at 0 CPU while holding the
    // sidecar mutex, wedging every VB extraction worker behind it. Both
    // failed OciusX reindexes died exactly here. The write now (a) checks
    // child liveness first, (b) caps the source size routed through the
    // sidecar, and (c) runs on a helper thread with the same timeout
    // discipline as the response read.
    if let Ok(Some(status)) = sidecar.child.try_wait() {
        return Err(SidecarParseError::Protocol(anyhow::anyhow!(
            "sidecar process exited ({status}) before request for {}",
            path.display()
        )));
    }

    let max_sidecar_bytes = std::env::var("ENGRAM_VB_SIDECAR_MAX_SOURCE_BYTES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(2_000_000);
    if source.len() > max_sidecar_bytes {
        return Err(SidecarParseError::Protocol(anyhow::anyhow!(
            "source too large for sidecar ({} bytes > {max_sidecar_bytes}) for {} — using fallback",
            source.len(),
            path.display()
        )));
    }

    let req = SidecarRequest {
        cmd: "parse",
        path: path.display().to_string(),
        source,
    };
    let payload = serde_json::to_string(&req).map_err(|e| SidecarParseError::Protocol(e.into()))?;

    let timeout_secs = std::env::var("ENGRAM_VB_SIDECAR_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(60);

    let mut stdin = sidecar.stdin.take().ok_or_else(|| {
        SidecarParseError::Protocol(anyhow::anyhow!(
            "sidecar stdin missing (previous write timed out)"
        ))
    })?;
    let (wtx, wrx) = mpsc::channel();
    std::thread::spawn(move || {
        let res = writeln!(stdin, "{payload}").and_then(|_| stdin.flush());
        let _ = wtx.send((stdin, res));
    });
    match wrx.recv_timeout(Duration::from_secs(timeout_secs)) {
        Ok((stdin, write_result)) => {
            sidecar.stdin = Some(stdin);
            write_result.map_err(|e| SidecarParseError::Protocol(e.into()))?;
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // Writer thread still blocked on the dead pipe: leave stdin
            // taken; the caller kills the child which unblocks the thread.
            return Err(SidecarParseError::Protocol(anyhow::anyhow!(
                "sidecar request write timed out after {timeout_secs}s for {}",
                path.display()
            )));
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return Err(SidecarParseError::Protocol(anyhow::anyhow!(
                "sidecar request write channel disconnected"
            )));
        }
    }
    let mut stdout = sidecar
        .stdout
        .take()
        .ok_or_else(|| SidecarParseError::Protocol(anyhow::anyhow!("sidecar stdout missing")))?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let read = stdout.read_line(&mut line).map(|_| line);
        let _ = tx.send((stdout, read));
    });
    let (stdout, line) = match rx.recv_timeout(Duration::from_secs(timeout_secs)) {
        Ok((stdout, read_result)) => (
            stdout,
            read_result.map_err(|e| SidecarParseError::Protocol(e.into()))?,
        ),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            return Err(SidecarParseError::Protocol(anyhow::anyhow!(
                "sidecar parse response timed out after {}s for {}",
                timeout_secs,
                path.display()
            )));
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return Err(SidecarParseError::Protocol(anyhow::anyhow!(
                "sidecar parse response channel disconnected"
            )));
        }
    };
    sidecar.stdout = Some(stdout);
    let response: SidecarResponse =
        serde_json::from_str(&line).map_err(|e| SidecarParseError::Protocol(e.into()))?;
    if let Some(error) = response.error {
        return Err(SidecarParseError::Sidecar(error));
    }

    let mut sidecar_edge_kind_counts: HashMap<String, usize> = HashMap::new();
    for edge in &response.edges {
        *sidecar_edge_kind_counts
            .entry(edge.kind.clone())
            .or_insert(0) += 1;
    }
    if !sidecar_edge_kind_counts.is_empty() {
        tracing::debug!(
            path = %path.display(),
            edge_kind_counts = ?sidecar_edge_kind_counts,
            "VB sidecar parse_via_sidecar deserialized edge counts"
        );
    }

    let symbols = response
        .symbols
        .into_iter()
        .map(|s| ExtractedSymbol {
            name: dedupe_fqn(&s.name),
            kind: s.kind,
            start_line: s.start_line,
            end_line: s.end_line,
            metadata: if s.metadata.is_empty() {
                None
            } else {
                Some(dedupe_fqn_metadata(s.metadata))
            },
        })
        .collect::<Vec<_>>();

    let edges = response
        .edges
        .into_iter()
        .map(|e| ExtractedEdge {
            source_name: dedupe_fqn(&e.source_name),
            source_kind: e.source_kind,
            source_start_line: e.source_start_line,
            source_language: e.source_language,
            target_name: dedupe_fqn(&e.target_name),
            target_kind: e.target_kind,
            target_start_line: e.target_start_line,
            kind: e.kind,
            metadata: if e.metadata.is_empty() {
                None
            } else {
                Some(dedupe_fqn_metadata(e.metadata))
            },
        })
        .collect::<Vec<_>>();

    Ok((symbols, edges))
}

/// Collapse repeated leading segment chains in a dotted name:
/// `_api2._api2.Logger.LogError` → `_api2.Logger.LogError`,
/// `a.b.c.a.b.c.X` → `a.b.c.X`. Older sidecar builds composed FQNs by
/// concatenating namespaces with an already-qualified type stack; this
/// normalizer makes ingestion correct regardless of deployed sidecar
/// version.
pub(crate) fn dedupe_fqn(name: &str) -> String {
    if !name.contains('.') {
        return name.to_string();
    }
    let mut parts: Vec<&str> = name.split('.').collect();
    loop {
        let mut collapsed = false;
        let max_k = parts.len() / 2;
        for k in (1..=max_k).rev() {
            if parts[..k] == parts[k..2 * k] {
                parts.drain(..k);
                collapsed = true;
                break;
            }
        }
        if !collapsed {
            break;
        }
    }
    parts.join(".")
}

/// Test-only re-export.
pub fn dedupe_fqn_for_test(name: &str) -> String {
    dedupe_fqn(name)
}

fn dedupe_fqn_metadata(mut m: HashMap<String, String>) -> HashMap<String, String> {
    if let Some(fqn) = m.get("fqn").cloned() {
        m.insert("fqn".to_string(), dedupe_fqn(&fqn));
    }
    m
}

pub fn begin_project(project_root: &Path) {
    let mutex = get_or_spawn_sidecar();
    let mut guard = match mutex.lock() {
        Ok(g) => g,
        Err(poisoned) => {
            tracing::error!("VB sidecar mutex poisoned during begin_project: {poisoned}");
            return;
        }
    };

    let sidecar = match ensure_sidecar(&mut guard) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("failed to spawn sidecar for begin_project: {e}");
            return;
        }
    };

    let req = serde_json::json!({
        "cmd": "begin_project",
        "project_root": project_root.display().to_string(),
    });

    // Same liveness discipline as parse_via_sidecar; begin_project sends a
    // tiny payload, so a blocking write cannot fill the pipe — the dead-child
    // check is the part that matters.
    if let Ok(Some(status)) = sidecar.child.try_wait() {
        tracing::warn!("begin_project: sidecar already exited ({status})");
        *guard = None;
        return;
    }
    let Some(stdin) = sidecar.stdin.as_mut() else {
        tracing::warn!("begin_project: sidecar stdin missing (prior write timed out)");
        let _ = sidecar.child.kill();
        *guard = None;
        return;
    };
    if let Err(e) = writeln!(stdin, "{}", req) {
        tracing::warn!("begin_project write failed: {e}");
        let _ = sidecar.child.kill();
        *guard = None;
        return;
    }
    if let Err(e) = stdin.flush() {
        tracing::warn!("begin_project flush failed: {e}");
        return;
    }

    let mut line = String::new();
    let Some(stdout) = sidecar.stdout.as_mut() else {
        tracing::warn!("begin_project response read failed: sidecar stdout missing");
        let _ = sidecar.child.kill();
        *guard = None;
        return;
    };
    if let Err(e) = stdout.read_line(&mut line) {
        tracing::warn!("begin_project response read failed: {e}");
        return;
    }
    if line.trim().is_empty() {
        tracing::warn!("begin_project returned empty response");
        let _ = sidecar.child.kill();
        *guard = None;
        return;
    }
    let parsed: serde_json::Value = match serde_json::from_str(&line) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("begin_project response parse failed: {e}");
            let _ = sidecar.child.kill();
            *guard = None;
            return;
        }
    };
    if let Some(err) = parsed.get("error").and_then(|v| v.as_str()) {
        tracing::warn!("begin_project sidecar error: {err}");
        let _ = sidecar.child.kill();
        *guard = None;
        return;
    }

    tracing::info!(
        "VB sidecar begin_project completed for {}",
        project_root.display()
    );
}

pub fn extract_vb(path: &Path, source: &str) -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>) {
    if source.bytes().all(|b| b.is_ascii_whitespace()) {
        return (Vec::new(), Vec::new());
    }

    let mutex = get_or_spawn_sidecar();
    let mut guard = match mutex.lock() {
        Ok(g) => g,
        Err(poisoned) => {
            tracing::error!("VB sidecar mutex poisoned: {poisoned}");
            return fallback_extract_vb(path, source);
        }
    };

    let sidecar = match ensure_sidecar(&mut guard) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("failed to spawn sidecar: {e}; using fallback VB extraction");
            return fallback_extract_vb(path, source);
        }
    };

    match parse_via_sidecar(sidecar, path, source) {
        Ok((mut symbols, mut edges)) => {
            // The Roslyn sidecar gives symbols/calls/SQL — but knows nothing
            // about Engram's settings/guard/hierarchy extraction. Enrich its
            // output with the same line-scan pass the fallback performs, so
            // production (sidecar) and degraded (fallback) modes agree.
            enrich_vb_source(source, &mut symbols, &mut edges);
            (symbols, edges)
        }
        Err(SidecarParseError::Sidecar(err)) => {
            tracing::warn!("VB sidecar parse error for {}: {err}", path.display());
            fallback_extract_vb(path, source)
        }
        Err(SidecarParseError::Protocol(err)) => {
            tracing::warn!("VB sidecar protocol failure for {}: {err}", path.display());
            let _ = sidecar.child.kill();
            *guard = None;
            fallback_extract_vb(path, source)
        }
    }
}

/// Test-only re-export: the enrichment pass can't be exercised through
/// `extract_vb` in tests (no sidecar binary), so integration tests call it
/// directly with synthetic real-ranged symbols.
pub fn enrich_vb_source_for_test(
    source: &str,
    symbols: &mut [ExtractedSymbol],
    edges: &mut Vec<ExtractedEdge>,
) {
    enrich_vb_source(source, symbols, edges)
}

/// Enrichment pass over sidecar output: settings reads (ReadsSetting edges),
/// permission-check metadata on the enclosing functions, and class-level
/// Inherits/Implements hierarchy edges. Sidecar symbols carry REAL line
/// ranges, so association is range-based (same approach as the C# path).
fn enrich_vb_source(source: &str, symbols: &mut [ExtractedSymbol], edges: &mut Vec<ExtractedEdge>) {
    // (start, end, name) for functions and classes, for range association.
    let fn_ranges: Vec<(u32, u32, String)> = symbols
        .iter()
        .filter(|s| s.kind == "function")
        .map(|s| (s.start_line, s.end_line, s.name.clone()))
        .collect();
    let class_ranges: Vec<(u32, u32, String)> = symbols
        .iter()
        .filter(|s| s.kind == "class")
        .map(|s| (s.start_line, s.end_line, s.name.clone()))
        .collect();
    let enclosing = |ranges: &[(u32, u32, String)], line: u32| -> Option<String> {
        ranges
            .iter()
            .filter(|(s, e, _)| *s <= line && *e >= line)
            // Innermost: the tightest containing range.
            .min_by_key(|(s, e, _)| e - s)
            .map(|(_, _, n)| n.clone())
    };

    // Avoid duplicating hierarchy edges the sidecar may already emit.
    let existing_hierarchy: std::collections::HashSet<(String, String)> = edges
        .iter()
        .filter(|e| e.kind == "inherits_from" || e.kind == "implements_interface")
        .map(|e| (e.source_name.clone(), e.target_name.clone()))
        .collect();

    let mut guard_hits: Vec<(u32, String)> = Vec::new();
    let mut role_hits: Vec<(u32, String)> = Vec::new();

    // ── Pass 0: collect ORM context variable names (file-scoped) ────────────
    let mut ctx_vars: std::collections::HashSet<String> = std::collections::HashSet::new();
    for line in source.lines() {
        for cap in RE_VB_CTX_DECL
            .captures_iter(line)
            .chain(RE_VB_CTX_ASSIGN.captures_iter(line))
        {
            if let Some(v) = cap.get(1) {
                ctx_vars.insert(v.as_str().to_lowercase());
            }
        }
    }
    // (function, table, access) — dedup before emitting; 900 LINQ queries in
    // one file must not become 900 identical edges.
    let mut table_accesses: std::collections::HashSet<(String, String, &'static str)> =
        std::collections::HashSet::new();
    let mut table_access_lines: std::collections::HashMap<(String, String), u32> =
        std::collections::HashMap::new();

    for (idx, raw_line) in source.lines().enumerate() {
        let line_no = idx as u32 + 1;
        let line = raw_line.trim();
        let lower = line.to_ascii_lowercase();

        // ── TODO-17: dynamic SQL on the sidecar path ───────────────────────
        // Roslyn's SQL detection (like the fallback's) only sees string
        // literals; variable command text was invisible in production.
        for dyn_cap in RE_VB_SQL_CMD_DYN
            .captures_iter(line)
            .chain(RE_VB_COMMAND_TEXT_DYN.captures_iter(line))
        {
            let Some(ident) = dyn_cap.get(1).map(|m| m.as_str()) else {
                continue;
            };
            if ident.eq_ignore_ascii_case("new") {
                continue;
            }
            let func = enclosing(&fn_ranges, line_no).unwrap_or_else(|| "file".to_string());
            let mut meta = std::collections::HashMap::new();
            meta.insert("dynamic".to_string(), "true".to_string());
            meta.insert("identifier".to_string(), ident.to_string());
            edges.push(ExtractedEdge {
                source_name: func,
                source_kind: "function".to_string(),
                source_start_line: line_no,
                source_language: "vb".to_string(),
                target_name: format!("sql:dynamic:{ident}"),
                target_kind: Some("inline_sql".to_string()),
                target_start_line: None,
                kind: "sql_calls".to_string(),
                metadata: Some(meta),
            });
        }

        // ── ORM table access (LINQ-to-SQL / EF) ────────────────────────────
        if !ctx_vars.is_empty() {
            for cap in RE_VB_CTX_WRITE.captures_iter(line) {
                let (Some(var), Some(table)) = (cap.get(1), cap.get(2)) else {
                    continue;
                };
                if !ctx_vars.contains(&var.as_str().to_lowercase()) {
                    continue;
                }
                let table_l = table.as_str().to_lowercase();
                if CTX_NON_TABLE_MEMBERS.contains(&table_l.as_str()) {
                    continue;
                }
                let func = enclosing(&fn_ranges, line_no).unwrap_or_else(|| "file".to_string());
                table_access_lines
                    .entry((func.clone(), table_l.clone()))
                    .or_insert(line_no);
                table_accesses.insert((func, table_l, "write"));
            }
            for cap in RE_VB_CTX_MEMBER.captures_iter(line) {
                let (Some(var), Some(table)) = (cap.get(1), cap.get(2)) else {
                    continue;
                };
                if !ctx_vars.contains(&var.as_str().to_lowercase()) {
                    continue;
                }
                // Method calls are not table properties: skip when the next
                // non-space char after the member is `(`.
                let after = line[table.end()..].trim_start();
                if after.starts_with('(') {
                    continue;
                }
                let table_l = table.as_str().to_lowercase();
                if CTX_NON_TABLE_MEMBERS.contains(&table_l.as_str()) {
                    continue;
                }
                let func = enclosing(&fn_ranges, line_no).unwrap_or_else(|| "file".to_string());
                table_access_lines
                    .entry((func.clone(), table_l.clone()))
                    .or_insert(line_no);
                table_accesses.insert((func, table_l, "read"));
            }
        }

        // Settings reads → edges from the enclosing function (or file).
        for cap in RE_VB_APPSETTINGS
            .captures_iter(line)
            .chain(RE_VB_MY_SETTINGS.captures_iter(line))
        {
            let Some(key) = cap.get(1).map(|m| m.as_str()) else {
                continue;
            };
            edges.push(ExtractedEdge {
                source_name: enclosing(&fn_ranges, line_no).unwrap_or_else(|| "file".to_string()),
                source_kind: "function".to_string(),
                source_start_line: line_no,
                source_language: "vb".to_string(),
                target_name: key.to_string(),
                target_kind: Some("app_setting".to_string()),
                target_start_line: None,
                kind: "reads_setting".to_string(),
                metadata: None,
            });
        }

        // Settings-store property reads (ConfigSettings.X.Y — see the
        // RE_VB_SETTINGS_STORE rationale above).
        for cap in RE_VB_SETTINGS_STORE.captures_iter(line) {
            let (Some(root), Some(tail)) = (cap.get(1), cap.get(2)) else {
                continue;
            };
            let root_l = root.as_str().to_lowercase();
            // Framework roots covered by the AppSettings shape; `My.Settings`
            // covered above; declarations like `Dim x As ConfigSettings` are
            // filtered by requiring a property tail (regex already does).
            if matches!(
                root_l.as_str(),
                "configurationmanager" | "webconfigurationmanager" | "configurationsettings"
            ) {
                continue;
            }
            // Method calls are not settings reads: skip when `(` follows.
            let after = line[tail.end()..].trim_start();
            if after.starts_with('(') {
                continue;
            }
            // Skip tails that are themselves store-plumbing members.
            let tail_l = tail.as_str().to_lowercase();
            if tail_l == "appsettings" || tail_l.starts_with("appsettings.") {
                continue;
            }
            edges.push(ExtractedEdge {
                source_name: enclosing(&fn_ranges, line_no).unwrap_or_else(|| "file".to_string()),
                source_kind: "function".to_string(),
                source_start_line: line_no,
                source_language: "vb".to_string(),
                target_name: format!("{}.{}", root.as_str(), tail.as_str()),
                target_kind: Some("app_setting".to_string()),
                target_start_line: None,
                kind: "reads_setting".to_string(),
                metadata: None,
            });
        }

        // Guard calls + role literals (annotated onto functions below).
        for cap in RE_VB_GUARD_CALL.captures_iter(line) {
            if let Some(name) = cap.get(1) {
                guard_hits.push((line_no, name.as_str().to_lowercase()));
            }
        }
        for cap in RE_VB_GUARD_ROLE_LITERAL.captures_iter(line) {
            if let Some(role) = cap.get(1) {
                role_hits.push((line_no, role.as_str().to_string()));
            }
        }

        // Class-level Inherits / Implements.
        if lower.starts_with("inherits ") || lower.starts_with("implements ") {
            let Some(class_name) = enclosing(&class_ranges, line_no) else {
                continue;
            };
            let is_implements = lower.starts_with("implements ");
            let list = &line[if is_implements { 11 } else { 9 }..];
            for raw_base in list.split(',') {
                let base = raw_base
                    .split('(')
                    .next()
                    .unwrap_or(raw_base)
                    .trim()
                    .trim_start_matches("Global.")
                    .trim();
                if base.is_empty()
                    || existing_hierarchy.contains(&(class_name.clone(), base.to_string()))
                {
                    continue;
                }
                edges.push(ExtractedEdge {
                    source_name: class_name.clone(),
                    source_kind: "class".to_string(),
                    source_start_line: line_no,
                    source_language: "vb".to_string(),
                    target_name: base.to_string(),
                    target_kind: Some("class".to_string()),
                    target_start_line: None,
                    kind: if is_implements {
                        "implements_interface".to_string()
                    } else {
                        "inherits_from".to_string()
                    },
                    metadata: None,
                });
            }
        }
    }

    // ── Emit deduplicated ORM table-access edges ────────────────────────────
    // One edge per (function, table); access metadata distinguishes
    // read / write / readwrite. Targets resolve to the DDL-extracted
    // db_table nodes via NodeId::table (lowercased) in ingest.
    let mut per_pair: std::collections::HashMap<(String, String), (bool, bool)> =
        std::collections::HashMap::new();
    for (func, table, access) in table_accesses {
        let entry = per_pair.entry((func, table)).or_insert((false, false));
        if access == "write" {
            entry.1 = true;
        } else {
            entry.0 = true;
        }
    }
    for ((func, table), (read, write)) in per_pair {
        let access = match (read, write) {
            (true, true) => "readwrite",
            (false, true) => "write",
            _ => "read",
        };
        let line = table_access_lines
            .get(&(func.clone(), table.clone()))
            .copied()
            .unwrap_or(0);
        let mut meta = HashMap::new();
        meta.insert("orm".to_string(), "true".to_string());
        meta.insert("access".to_string(), access.to_string());
        edges.push(ExtractedEdge {
            source_name: func,
            source_kind: "function".to_string(),
            source_start_line: line,
            source_language: "vb".to_string(),
            target_name: table,
            target_kind: Some("db_table".to_string()),
            target_start_line: None,
            kind: "queries_table".to_string(),
            metadata: Some(meta),
        });
    }

    // Range-based guard annotation (shared with the C# extractor).
    crate::cs_extractor::annotate_guards(symbols, &guard_hits, &role_hits);
}

/// Detect a VB.NET type declaration on a line and return (graph node-kind, name).
/// `Module`/`Structure`/`Enum` are treated as `class`-kind containers so their
/// members get a correct `Type.Member` FQN + `contains` edge and participate in
/// all class-based navigation/blast-radius logic; `Interface` keeps its kind.
/// Word-boundary matched (surrounding spaces) so `Enumerable`, `X.Class`, and
/// `End Class` don't false-positive.
fn detect_vb_type_decl(line: &str) -> Option<(&'static str, String)> {
    let lower = line.to_ascii_lowercase();
    if lower.trim_start().starts_with("end ") {
        return None;
    }
    const KW: &[(&str, &str)] = &[
        ("class", "class"),
        ("module", "class"),
        ("structure", "class"),
        ("interface", "interface"),
        ("enum", "class"),
    ];
    for (kw, kind) in KW {
        if lower.contains(&format!(" {kw} ")) || lower.starts_with(&format!("{kw} ")) {
            let mut name = line
                .split_whitespace()
                .skip_while(|t| t.to_ascii_lowercase() != *kw)
                .nth(1)
                .unwrap_or_default()
                .to_string();
            if let Some(p) = name.find('(') {
                name.truncate(p); // drop generic params: Foo(Of T) -> Foo
            }
            if name
                .chars()
                .next()
                .map(|c| c.is_alphabetic() || c == '_')
                .unwrap_or(false)
            {
                return Some((kind, name));
            }
        }
    }
    None
}

fn fallback_extract_vb(_path: &Path, source: &str) -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>) {
    // Lightweight fallback if sidecar isn't available.
    let mut symbols = Vec::new();
    let mut edges = Vec::new();
    let mut ns = String::new();
    let mut ty = String::new();
    // FQN of the enclosing Sub/Function — sql_calls edges must name their
    // real source symbol (matching the FQN-named node minted below) or fall
    // back to the "file" sentinel.
    let mut current_method: Option<String> = None;
    // Guard calls per enclosing method: (guard names, role literals).
    let mut method_guards: HashMap<String, (Vec<String>, Vec<String>)> = HashMap::new();

    let mut add_symbol =
        |name: String, kind: &str, line: u32, metadata: Option<HashMap<String, String>>| {
            symbols.push(ExtractedSymbol {
                name,
                kind: kind.to_string(),
                start_line: line,
                end_line: line,
                metadata,
            });
        };

    for (idx, raw_line) in source.lines().enumerate() {
        let line_no = idx as u32 + 1;
        let line = raw_line.trim();
        let lower = line.to_ascii_lowercase();

        // SQL detection runs on every line (independent of the declaration
        // branches below): New SqlCommand("...") and .CommandText = "...".
        for sql_cap in RE_VB_SQL_CMD
            .captures_iter(line)
            .chain(RE_VB_COMMAND_TEXT.captures_iter(line))
        {
            let sql = sql_cap.get(1).map(|m| m.as_str()).unwrap_or("");
            if sql.trim().is_empty() {
                continue;
            }
            let (target_name, target_kind) = crate::cs_extractor::classify_cs_sql(sql);
            let mut meta = HashMap::new();
            meta.insert(
                "sql_snippet".to_string(),
                sql.chars().take(200).collect::<String>(),
            );
            // TODO-17: literal head + string concat ("SELECT ... " & var)
            // means the real statement is built at runtime — keep the edge
            // (the head still names tables) but mark it.
            if let Some(end) = sql_cap.get(1).map(|m| m.end())
                && line[end..].trim_start().starts_with('"')
                && line[end + 1..].trim_start().starts_with('&')
            {
                meta.insert("dynamic".to_string(), "true".to_string());
            }
            edges.push(ExtractedEdge {
                source_name: current_method.clone().unwrap_or_else(|| "file".to_string()),
                source_kind: "function".to_string(),
                source_start_line: line_no,
                source_language: "vb".to_string(),
                target_name,
                target_kind: Some(target_kind.to_string()),
                target_start_line: None,
                kind: "sql_calls".to_string(),
                metadata: Some(meta),
            });
        }

        // TODO-17: dynamic SQL (variable command text) — emit a marked edge
        // instead of dropping the access entirely.
        for dyn_cap in RE_VB_SQL_CMD_DYN
            .captures_iter(line)
            .chain(RE_VB_COMMAND_TEXT_DYN.captures_iter(line))
        {
            let Some(ident) = dyn_cap.get(1).map(|m| m.as_str()) else {
                continue;
            };
            // Skip literal matches re-captured by the loose pattern.
            if ident.eq_ignore_ascii_case("new") {
                continue;
            }
            let mut meta = HashMap::new();
            meta.insert("dynamic".to_string(), "true".to_string());
            meta.insert("identifier".to_string(), ident.to_string());
            edges.push(ExtractedEdge {
                source_name: current_method.clone().unwrap_or_else(|| "file".to_string()),
                source_kind: "function".to_string(),
                source_start_line: line_no,
                source_language: "vb".to_string(),
                target_name: format!("sql:dynamic:{ident}"),
                target_kind: Some("inline_sql".to_string()),
                target_start_line: None,
                kind: "sql_calls".to_string(),
                metadata: Some(meta),
            });
        }

        // Settings reads (web.config appSettings + My.Settings members).
        for cap in RE_VB_APPSETTINGS
            .captures_iter(line)
            .chain(RE_VB_MY_SETTINGS.captures_iter(line))
        {
            let Some(key) = cap.get(1).map(|m| m.as_str()) else {
                continue;
            };
            edges.push(ExtractedEdge {
                source_name: current_method.clone().unwrap_or_else(|| "file".to_string()),
                source_kind: "function".to_string(),
                source_start_line: line_no,
                source_language: "vb".to_string(),
                target_name: key.to_string(),
                target_kind: Some("app_setting".to_string()),
                target_start_line: None,
                kind: "reads_setting".to_string(),
                metadata: None,
            });
        }

        // Settings-store property reads (ConfigSettings.X.Y) — same shape as
        // the sidecar path; see RE_VB_SETTINGS_STORE.
        for cap in RE_VB_SETTINGS_STORE.captures_iter(line) {
            let (Some(root), Some(tail)) = (cap.get(1), cap.get(2)) else {
                continue;
            };
            let root_l = root.as_str().to_lowercase();
            if matches!(
                root_l.as_str(),
                "configurationmanager" | "webconfigurationmanager" | "configurationsettings"
            ) {
                continue;
            }
            let after = line[tail.end()..].trim_start();
            if after.starts_with('(') {
                continue;
            }
            let tail_l = tail.as_str().to_lowercase();
            if tail_l == "appsettings" || tail_l.starts_with("appsettings.") {
                continue;
            }
            edges.push(ExtractedEdge {
                source_name: current_method.clone().unwrap_or_else(|| "file".to_string()),
                source_kind: "function".to_string(),
                source_start_line: line_no,
                source_language: "vb".to_string(),
                target_name: format!("{}.{}", root.as_str(), tail.as_str()),
                target_kind: Some("app_setting".to_string()),
                target_start_line: None,
                kind: "reads_setting".to_string(),
                metadata: None,
            });
        }

        // Permission checks: collected per enclosing method, annotated after
        // the scan (fallback symbols are line-anchored, so association is by
        // the current-method context rather than line ranges).
        if let Some(ref method_fqn) = current_method {
            for cap in RE_VB_GUARD_CALL.captures_iter(line) {
                if let Some(name) = cap.get(1) {
                    method_guards
                        .entry(method_fqn.clone())
                        .or_default()
                        .0
                        .push(name.as_str().to_lowercase());
                }
            }
            for cap in RE_VB_GUARD_ROLE_LITERAL.captures_iter(line) {
                if let Some(role) = cap.get(1) {
                    method_guards
                        .entry(method_fqn.clone())
                        .or_default()
                        .1
                        .push(role.as_str().to_string());
                }
            }
        }

        // Qualified call edges (Foo.Bar(...)) inside method bodies keep the
        // handler → DAL → SQL chain connected when the sidecar is absent.
        // Targets go out unresolved (target_kind: None → "::name") so the
        // post-ingest resolver matches them by terminal segment.
        if let Some(ref method_fqn) = current_method {
            for c in RE_VB_QUALIFIED_CALL.captures_iter(line) {
                if c.get(1).is_some() {
                    continue; // constructor: New Foo.Bar(...)
                }
                let callee = c.get(2).map(|m| m.as_str()).unwrap_or("");
                let head = callee.split('.').next().unwrap_or("").to_ascii_lowercase();
                if callee.is_empty() || VB_CALL_HEAD_STOPWORDS.contains(&head.as_str()) {
                    continue;
                }
                edges.push(ExtractedEdge {
                    source_name: method_fqn.clone(),
                    source_kind: "function".to_string(),
                    source_start_line: line_no,
                    source_language: "vb".to_string(),
                    target_name: callee.to_string(),
                    target_kind: None,
                    target_start_line: None,
                    kind: "calls".to_string(),
                    metadata: None,
                });
            }
        }

        if lower.starts_with("end sub") || lower.starts_with("end function") {
            current_method = None;
        }

        if lower.starts_with("namespace ") {
            ns = line
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_string();
        } else if let Some((kind, name)) = detect_vb_type_decl(line) {
            // class / module / structure / interface / enum all open a type
            // scope. VB.NET uses `Module` pervasively for shared utility code,
            // and Structure/Interface/Enum were previously invisible to the
            // fallback — so their Subs/Functions lost both their `Type.Member`
            // FQN and the `contains` edge to their container (breaking
            // blast-radius and contains-based navigation in degraded mode).
            ty = name.clone();
            add_symbol(name.clone(), kind, line_no, None);
            if !ns.is_empty() {
                edges.push(ExtractedEdge {
                    source_name: ns.clone(),
                    source_kind: "namespace".to_string(),
                    source_start_line: 1,
                    source_language: "vb".to_string(),
                    target_name: name,
                    target_kind: Some(kind.to_string()),
                    target_start_line: Some(line_no),
                    kind: "contains".to_string(),
                    metadata: None,
                });
            }
        } else if let Some(we) = RE_VB_WITHEVENTS.captures(line) {
            if let Some(name_m) = we.get(1) {
                let name = name_m.as_str().to_string();
                add_symbol(name.clone(), "control_ref", line_no, None);
                if !ty.is_empty() {
                    edges.push(ExtractedEdge {
                        source_name: ty.clone(),
                        source_kind: "class".to_string(),
                        source_start_line: line_no,
                        source_language: "vb".to_string(),
                        target_name: name,
                        target_kind: Some("control_ref".to_string()),
                        target_start_line: Some(line_no),
                        kind: "contains".to_string(),
                        metadata: None,
                    });
                }
            }
        } else if lower.contains(" sub ")
            || lower.starts_with("sub ")
            || lower.contains(" function ")
            || lower.starts_with("function ")
        {
            let tokens: Vec<_> = line.split_whitespace().collect();
            let kw_idx = tokens
                .iter()
                .position(|t| matches!(t.to_ascii_lowercase().as_str(), "sub" | "function"));
            if let Some(i) = kw_idx {
                let mut method = tokens.get(i + 1).copied().unwrap_or_default().to_string();
                if let Some(p) = method.find('(') {
                    method.truncate(p);
                }
                let mut meta = HashMap::new();
                // TODO-13: parameter count from the signature line so the
                // degraded path also feeds arity-aware resolution. Counts
                // top-level commas inside the first paren group; nested
                // parens (generics, defaults) are tracked by depth.
                if let Some(open) = line.find('(') {
                    let mut depth = 0i32;
                    let mut args: u32 = 0;
                    let mut saw_content = false;
                    for ch in line[open..].chars() {
                        match ch {
                            '(' => depth += 1,
                            ')' => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            ',' if depth == 1 => args += 1,
                            c if depth >= 1 && !c.is_whitespace() => saw_content = true,
                            _ => {}
                        }
                    }
                    let arity = if saw_content { args + 1 } else { 0 };
                    meta.insert("arity".to_string(), arity.to_string());
                }
                if lower.contains("async ") {
                    meta.insert("async".to_string(), "true".to_string());
                }
                if let Some((stage, seq)) = webforms_lifecycle_info(&method) {
                    meta.insert("lifecycle_stage".to_string(), stage.to_string());
                    meta.insert("lifecycle_sequence".to_string(), seq.to_string());
                }
                let fqn = if ns.is_empty() {
                    if ty.is_empty() {
                        method.clone()
                    } else {
                        format!("{ty}.{method}")
                    }
                } else if ty.is_empty() {
                    format!("{ns}.{method}")
                } else {
                    format!("{ns}.{ty}.{method}")
                };
                add_symbol(
                    fqn.clone(),
                    "function",
                    line_no,
                    if meta.is_empty() {
                        None
                    } else {
                        Some(meta.clone())
                    },
                );
                if !ty.is_empty() {
                    edges.push(ExtractedEdge {
                        source_name: ty.clone(),
                        source_kind: "class".to_string(),
                        source_start_line: line_no,
                        source_language: "vb".to_string(),
                        target_name: fqn.clone(),
                        target_kind: Some("function".to_string()),
                        target_start_line: Some(line_no),
                        kind: "contains".to_string(),
                        metadata: None,
                    });
                }
                current_method = Some(fqn.clone());
                if let Some(handles_pos) = lower.find(" handles ") {
                    let handles = &line[handles_pos + 9..];
                    for part in handles.split(',') {
                        let part = part.trim();
                        if let Some((control, _evt)) = part.split_once('.') {
                            let mut m = HashMap::new();
                            m.insert("fqn".to_string(), fqn.clone());
                            edges.push(ExtractedEdge {
                                source_name: control.trim().to_string(),
                                source_kind: "control".to_string(),
                                source_start_line: line_no,
                                source_language: "vb".to_string(),
                                target_name: method.clone(),
                                target_kind: Some("function".to_string()),
                                target_start_line: Some(line_no),
                                kind: "event_wiring".to_string(),
                                metadata: Some(m),
                            });
                        }
                    }
                }
            }
        } else if (lower.starts_with("inherits ") || lower.starts_with("implements "))
            && !ty.is_empty()
        {
            // Type hierarchy: class-level Inherits / Implements statements.
            let is_implements = lower.starts_with("implements ");
            let list = &line[if is_implements { 11 } else { 9 }..];
            for raw_base in list.split(',') {
                let base = raw_base
                    .split('(')
                    .next()
                    .unwrap_or(raw_base)
                    .trim()
                    .trim_start_matches("Global.")
                    .trim();
                if base.is_empty() {
                    continue;
                }
                edges.push(ExtractedEdge {
                    source_name: ty.clone(),
                    source_kind: "class".to_string(),
                    source_start_line: line_no,
                    source_language: "vb".to_string(),
                    target_name: base.to_string(),
                    target_kind: Some("class".to_string()),
                    target_start_line: None,
                    kind: if is_implements {
                        "implements_interface".to_string()
                    } else {
                        "inherits_from".to_string()
                    },
                    metadata: None,
                });
            }
        } else if lower.starts_with("imports ") {
            let target = line[8..].trim().to_string();
            edges.push(ExtractedEdge {
                // "file" sentinel: ingest substitutes the file's project-relative
                // path. Passing path.display() here put an ABSOLUTE path into
                // the edge source, which process_ingest_stats rejects with a
                // bail! — aborting ingestion of the ENTIRE batch for any VB
                // project containing a single Imports statement.
                source_name: "file".to_string(),
                source_kind: "file".to_string(),
                source_start_line: line_no,
                source_language: "vb".to_string(),
                target_name: target,
                target_kind: Some("namespace".to_string()),
                target_start_line: None,
                kind: "imports".to_string(),
                metadata: None,
            });
        }
    }

    // Attach collected guard facts to the (FQN-named) function symbols.
    for sym in symbols.iter_mut() {
        if sym.kind != "function" {
            continue;
        }
        let Some((guards, roles)) = method_guards.get(&sym.name) else {
            continue;
        };
        let mut guards = guards.clone();
        guards.sort();
        guards.dedup();
        let mut roles = roles.clone();
        roles.sort();
        roles.dedup();
        let mut meta = sym.metadata.take().unwrap_or_default();
        if !guards.is_empty() {
            meta.insert("permission_checks".to_string(), guards.join(";"));
        }
        if !roles.is_empty() {
            meta.insert("guard_roles".to_string(), roles.join(";"));
        }
        sym.metadata = Some(meta);
    }

    // Backfill end_line for Sub/Function bodies. The line-scan emits each method
    // with end_line == start_line (it can't know the end at the declaration
    // line); without this every VB method shows a 1-line range in degraded
    // (no-sidecar) mode, breaking get_method_edit_context / read-lines body
    // retrieval. VB Subs/Functions don't nest, so each body ends at the first
    // `End Sub`/`End Function` at-or-after its start.
    let method_ends: Vec<u32> = source
        .lines()
        .enumerate()
        .filter_map(|(i, l)| {
            let lo = l.trim().to_ascii_lowercase();
            (lo.starts_with("end sub") || lo.starts_with("end function")).then_some(i as u32 + 1)
        })
        .collect();
    for s in &mut symbols {
        if s.kind == "function"
            && let Some(&end) = method_ends.iter().find(|&&e| e >= s.start_line)
            && end > s.start_line
        {
            s.end_line = end;
        }
    }

    // TODO-18: tag degraded-mode output. The sidecar silently falls back to
    // this line-scan extractor on spawn/protocol failure; without a marker
    // the lower-fidelity symbols are indistinguishable from Roslyn output,
    // so confidence scoring and audits can't discount them.
    for s in &mut symbols {
        s.metadata
            .get_or_insert_with(Default::default)
            .insert("extraction_fallback".to_string(), "true".to_string());
    }

    (symbols, edges)
}

/// Eval/test entry to the degraded-mode extractor: deterministic (no
/// sidecar dependency), used by the graph-accuracy eval (TODO-49) and
/// unit tests. Not part of the indexing API.
#[doc(hidden)]
pub fn extract_vb_fallback_for_eval(
    path: &Path,
    source: &str,
) -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>) {
    let (mut symbols, mut edges) = fallback_extract_vb(path, source);
    // Run the shared enrichment pass too (ORM/hierarchy/etc.) — it dedups
    // against already-emitted edges, mirroring full pipeline behavior.
    enrich_vb_source(source, &mut symbols, &mut edges);
    (symbols, edges)
}

/// Test-only re-export of the degraded-mode extractor so tests can pin its
/// behavior without depending on sidecar availability in the environment.
#[cfg(test)]
pub(crate) fn fallback_extract_vb_for_test(
    path: &Path,
    source: &str,
) -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>) {
    fallback_extract_vb(path, source)
}

fn webforms_lifecycle_info(name: &str) -> Option<(&'static str, &'static str)> {
    match name.to_ascii_lowercase().as_str() {
        "page_preinit" => Some(("PreInit", "1")),
        "page_init" | "oninit" => Some(("Init", "2")),
        "page_preload" => Some(("PreLoad", "4")),
        "page_load" | "onload" => Some(("Load", "5")),
        "page_prerender" | "onprerender" => Some(("PreRender", "7")),
        "page_unload" | "onunload" => Some(("Unload", "11")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::extract_vb;
    use std::path::Path;

    #[test]
    fn guard_regex_matches_checkreadwrite_and_project_access_families() {
        let re = &*super::RE_VB_GUARD_CALL;
        // Previously MISSED (no access/permission/role substring) — the
        // guard-map DAO-delegation blind spot.
        assert!(re.is_match("If _us.UserAccess.CheckWrite(obj) Then"));
        assert!(re.is_match("If _us.UserAccess.CheckRead(obj) Then"));
        assert!(re.is_match("_us.accessctrl.Check_pr_id(project_id)"));
        assert!(re.is_match("_us.accessctrl.Check_rv_id(rv_id)"));
        // Still matched via the existing "access" alternative.
        assert!(re.is_match("check_fiberaccessbyid(x)"));
        assert!(re.is_match("If IsUserInRole(\"Admin\") Then"));
        // Must NOT match ordinary calls.
        assert!(!re.is_match("Dim n = GetCount(list)"));
        assert!(!re.is_match("results.CheckSum(data)"));
    }

    #[test]
    fn settings_store_property_reads_emit_reads_setting_edges() {
        // The house pattern in mature apps: static store classes, not raw
        // AppSettings("..."). Missing this shape made settings intelligence
        // blind on exactly the codebases that need it.
        let code = "Class P\n  Public Sub Page_Load()\n    If ConfigSettings.Multitenant.IsMaster Then\n    End If\n    Dim x = SystemSettingStore.General.RoqEnableListTypeDimension\n    Dim y = ConfigurationManager.AppSettings(\"PlainKey\")\n    Dim z = SettingsHelper.Load(\"skip-method-calls\")\n    If Not _us.UserAccess.CheckWrite(_us.UserAccessObject.approval_of_inspection) Then\n    End If\n  End Sub\nEnd Class";
        let (_, edges) = super::fallback_extract_vb_for_test(Path::new("p.aspx.vb"), code);
        let settings: Vec<&str> = edges
            .iter()
            .filter(|e| e.kind == "reads_setting")
            .map(|e| e.target_name.as_str())
            .collect();
        assert!(
            settings.contains(&"ConfigSettings.Multitenant.IsMaster"),
            "{settings:?}"
        );
        assert!(
            settings
                .iter()
                .any(|s| s.starts_with("SystemSettingStore.General.RoqEnable")),
            "{settings:?}"
        );
        assert!(settings.contains(&"PlainKey"), "{settings:?}");
        assert!(
            !settings
                .iter()
                .any(|s| s.contains("skip-method-calls") || s.ends_with(".Load")),
            "method calls must not become settings reads: {settings:?}"
        );
        // Token-INITIAL roots (UserAccessObject) regressed silently when the
        // root pattern required a char before the token; permission reads
        // never extracted.
        assert!(
            settings.contains(&"UserAccessObject.approval_of_inspection"),
            "token-initial roots must extract: {settings:?}"
        );
        assert!(
            !settings.iter().any(|s| s.ends_with(".CheckWrite")),
            "method calls on access helpers must not extract: {settings:?}"
        );
        assert!(
            !settings
                .iter()
                .any(|s| s.starts_with("ConfigurationManager.")),
            "framework root must stay excluded: {settings:?}"
        );
    }

    #[test]
    fn fallback_symbols_are_tagged_extraction_fallback() {
        // TODO-18: degraded-mode output must be distinguishable from
        // Roslyn sidecar output so confidence scoring can discount it.
        let code = "Class Foo\n  Public Sub Bar()\n  End Sub\nEnd Class";
        let (symbols, _) = super::fallback_extract_vb_for_test(Path::new("foo.vb"), code);
        assert!(!symbols.is_empty());
        for s in &symbols {
            assert_eq!(
                s.metadata
                    .as_ref()
                    .and_then(|m| m.get("extraction_fallback"))
                    .map(String::as_str),
                Some("true"),
                "symbol {} missing extraction_fallback tag",
                s.name
            );
        }
    }

    #[test]
    fn enrich_marks_dynamic_sql_on_sidecar_path() {
        let code = "Class D
  Sub Run()
    cmd.CommandText = dynQry
  End Sub
End Class";
        let mut symbols = vec![crate::parsing::ExtractedSymbol {
            name: "D.Run".to_string(),
            kind: "function".to_string(),
            start_line: 2,
            end_line: 4,
            metadata: None,
        }];
        let mut edges = Vec::new();
        super::enrich_vb_source_for_test(code, &mut symbols, &mut edges);
        let dyn_edge = edges
            .iter()
            .find(|e| e.target_name == "sql:dynamic:dynQry")
            .expect("dynamic sql edge from enrichment");
        assert_eq!(dyn_edge.source_name, "D.Run", "attributed to enclosing fn");
        assert!(
            dyn_edge
                .metadata
                .as_ref()
                .is_some_and(|m| m.get("dynamic").is_some_and(|v| v == "true"))
        );
    }

    #[test]
    fn dynamic_sql_is_marked_not_dropped() {
        let code = "Class D
  Sub Run()
    Dim cmd As New SqlCommand(strSql)
    cmd.CommandText = dynamicQuery
  End Sub
End Class";
        let (_, edges) = super::fallback_extract_vb_for_test(Path::new("d.vb"), code);
        let dyn_edges: Vec<_> = edges
            .iter()
            .filter(|e| {
                e.kind == "sql_calls"
                    && e.metadata
                        .as_ref()
                        .is_some_and(|m| m.get("dynamic").is_some_and(|v| v == "true"))
            })
            .collect();
        assert_eq!(
            dyn_edges.len(),
            2,
            "both variable-SQL sites must emit: {edges:?}"
        );
        assert!(
            dyn_edges
                .iter()
                .any(|e| e.target_name == "sql:dynamic:strSql")
        );
        assert!(
            dyn_edges
                .iter()
                .any(|e| e.target_name == "sql:dynamic:dynamicQuery")
        );
    }

    #[test]
    fn fallback_records_arity_from_signature() {
        let code = "Class Foo
  Public Sub NoArgs()
  End Sub
  Public Function TwoArgs(ByVal a As Integer, b As String) As Boolean
  End Function
  Public Sub Defaulted(Optional ByVal x As List(Of String) = Nothing)
  End Sub
End Class";
        let (symbols, _) = super::fallback_extract_vb_for_test(Path::new("a.vb"), code);
        let arity_of = |name: &str| -> Option<String> {
            symbols
                .iter()
                .find(|s| s.name.contains(name))
                .and_then(|s| s.metadata.as_ref())
                .and_then(|m| m.get("arity"))
                .cloned()
        };
        assert_eq!(arity_of("NoArgs").as_deref(), Some("0"));
        assert_eq!(arity_of("TwoArgs").as_deref(), Some("2"));
        assert_eq!(
            arity_of("Defaulted").as_deref(),
            Some("1"),
            "nested parens (List(Of String)) must not inflate the count"
        );
    }

    #[test]
    fn indexes_partial_class_without_modifier() {
        let code = "Partial Class Foo\n  Public Sub Bar()\n  End Sub\nEnd Class";
        let (symbols, _) = extract_vb(Path::new("foo.vb"), code);
        assert!(symbols.iter().any(|s| s.name.contains("Foo")));
        assert!(symbols.iter().any(|s| s.name.contains("Bar")));
    }

    #[test]
    fn indexes_async_function() {
        let code =
            "Class Foo\n  Async Function Bar() As Task(Of String)\n  End Function\nEnd Class";
        let (symbols, _) = extract_vb(Path::new("foo.vb"), code);
        let bar = symbols.iter().find(|s| s.name.contains("Bar")).unwrap();
        let md = bar.metadata.as_ref().unwrap();
        assert_eq!(md.get("async").map(String::as_str), Some("true"));
    }

    #[test]
    fn parses_interpolation_and_null_conditional() {
        let code = "Class Foo\n Sub Bar()\n   Dim x = app?.Name\n   Dim s = $\"Hello {name}\"\n End Sub\nEnd Class";
        let (symbols, _) = extract_vb(Path::new("foo.vb"), code);
        assert!(symbols.iter().any(|s| s.name.contains("Bar")));
    }

    #[test]
    fn fallback_backfills_method_end_line() {
        // Degraded mode must give a real body range, not start==end (else
        // get_method_edit_context shows a 1-line VB method).
        let code = "Module M\n  Public Sub Bar()\n    Dim x = 1\n    x = 2\n  End Sub\nEnd Module";
        let (symbols, _) = super::fallback_extract_vb_for_test(Path::new("m.vb"), code);
        let bar = symbols
            .iter()
            .find(|s| s.name.contains("Bar"))
            .expect("Bar extracted");
        assert_eq!(bar.start_line, 2, "start at Sub line");
        assert_eq!(bar.end_line, 5, "end at End Sub line");
        assert!(bar.end_line > bar.start_line);
    }

    #[test]
    fn sidecar_parse_smoke_when_enabled() {
        if std::env::var("ENGRAM_VB_SIDECAR_TEST").ok().as_deref() != Some("1") {
            return;
        }
        let sidecar_path = super::sidecar_binary_path();
        if !sidecar_path.exists() {
            return;
        }

        let code = "Namespace App_Code\nPublic Class SharedFunc\nPublic Sub SafeRedirect()\nEnd Sub\nEnd Class\nEnd Namespace";
        let (symbols, edges) = extract_vb(Path::new("App_Code/SharedFunc.vb"), code);
        assert!(!symbols.is_empty(), "expected sidecar/fallback symbols");
        assert!(edges.iter().any(|e| e.kind == "contains"));
    }
}
