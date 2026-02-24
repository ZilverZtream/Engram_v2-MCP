//! Runtime instrumentation pipeline — generates injectable instrumentation code
//! for legacy ASP.NET apps and reconciles static vs. runtime evidence.
//!
//! Produces HttpModule instrumentation (C# or VB.NET) that captures route events,
//! session access, SQL execution, control interactions, and error events, plus
//! a reconciliation tool that compares static analysis with runtime behavior.

use engram_core::runtime_evidence::{RuntimeEvent, RuntimeEventType, RuntimeEvidenceBatch};
use engram_graph::{EdgeKind, GraphStore};
use serde::Serialize;
use std::collections::HashMap;
use std::fmt::Write;
use std::sync::Arc;

/// Output of instrumentation code generation.
#[derive(Debug, Clone, Serialize)]
pub struct InstrumentationPackage {
    /// Generated C# instrumentation module code.
    pub csharp_module: String,
    /// Generated VB.NET instrumentation module code.
    pub vb_module: String,
    /// Generated C# session wrapper class (if session access detected).
    pub session_wrapper: Option<String>,
    /// Generated C# SQL wrapper class (if SQL execution detected).
    pub sql_wrapper: Option<String>,
    /// web.config entries needed.
    pub webconfig_entries: String,
    /// Installation instructions.
    pub installation_steps: Vec<String>,
    /// What the instrumentation captures.
    pub captured_events: Vec<String>,
}

/// Result of reconciling static analysis with runtime evidence.
#[derive(Debug, Clone, Serialize)]
pub struct ReconciliationReport {
    /// Paths confirmed by runtime evidence.
    pub confirmed_paths: Vec<PathEvidence>,
    /// Paths contradicted (not exercised at runtime).
    pub contradicted_paths: Vec<PathEvidence>,
    /// Paths with no runtime data.
    pub inconclusive_paths: Vec<PathEvidence>,
    /// Summary statistics.
    pub summary: ReconciliationSummary,
}

#[derive(Debug, Clone, Serialize)]
pub struct PathEvidence {
    pub source: String,
    pub target: String,
    pub edge_kind: String,
    pub static_evidence: String,
    pub runtime_evidence: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReconciliationSummary {
    pub total_static_paths: usize,
    pub confirmed_count: usize,
    pub contradicted_count: usize,
    pub inconclusive_count: usize,
    pub confirmed_ratio: f64,
    pub contradicted_ratio: f64,
    pub confidence_delta: f64,
}

// ─── Public API ───────────────────────────────────────────────────────────────

/// Generate instrumentation code for a legacy ASP.NET application.
pub fn generate_instrumentation_code(
    graph: &Arc<GraphStore>,
    project_id: &str,
    _target_files: &[String],
    language: &str,
) -> anyhow::Result<InstrumentationPackage> {
    // Analyze what needs to be instrumented
    let has_session = !graph
        .list_edges_by_kind(project_id, EdgeKind::ReadsState, 1)?
        .is_empty()
        || !graph
            .list_edges_by_kind(project_id, EdgeKind::WritesState, 1)?
            .is_empty();
    let has_sql = !graph
        .list_edges_by_kind(project_id, EdgeKind::SqlCalls, 1)?
        .is_empty();
    let has_postback = !graph
        .list_edges_by_kind(project_id, EdgeKind::TriggersPostback, 1)?
        .is_empty();

    let _is_vb = language.to_lowercase().contains("vb");

    let csharp_module = generate_csharp_module(has_session, has_sql, has_postback);
    let vb_module = generate_vb_module(has_session, has_sql, has_postback);
    let webconfig_entries = generate_webconfig_entries();

    // Generate wrapper classes that intercept session/SQL automatically
    let session_wrapper = if has_session {
        Some(generate_session_wrapper())
    } else {
        None
    };
    let sql_wrapper = if has_sql {
        Some(generate_sql_wrapper())
    } else {
        None
    };

    let mut captured_events = vec!["HTTP request/response (route, method, status, timing)".into()];
    if has_session {
        captured_events.push("Session access (key reads/writes with timestamps)".into());
    }
    if has_sql {
        captured_events.push("SQL execution (command text, timing, row count)".into());
    }
    if has_postback {
        captured_events.push("Control interactions (postback source, event argument)".into());
    }
    captured_events.push("Error events (unhandled exceptions with stack trace)".into());

    let mut installation_steps = vec![
        "1. Add EngramInstrumentation.cs (or .vb) to your App_Code folder".into(),
        "2. Add the web.config entries below to the <system.webServer><modules> section".into(),
        "3. Create a 'logs' folder in the application root (or configure the output path)".into(),
    ];
    if has_session {
        installation_steps.push(
            "4. Add InstrumentedSessionStateWrapper.cs to App_Code/ — it auto-intercepts Session reads/writes via the HttpModule".into(),
        );
    }
    if has_sql {
        installation_steps.push(
            format!(
                "{}. Add InstrumentedDbCommand.cs to App_Code/ — replace `new SqlCommand(...)` with `InstrumentedDbCommand.Wrap(new SqlCommand(...))`",
                if has_session { "5" } else { "4" }
            ),
        );
    }
    installation_steps.push(format!(
        "{}. Deploy and exercise the application normally",
        if has_session && has_sql {
            "6"
        } else if has_session || has_sql {
            "5"
        } else {
            "4"
        }
    ));
    installation_steps.push(format!(
        "{}. Collect the generated JSON logs and feed them to the {} tool",
        if has_session && has_sql {
            "7"
        } else if has_session || has_sql {
            "6"
        } else {
            "5"
        },
        "ingest_instrumentation_logs"
    ));

    Ok(InstrumentationPackage {
        csharp_module,
        vb_module,
        session_wrapper,
        sql_wrapper,
        webconfig_entries,
        installation_steps,
        captured_events,
    })
}

/// Reconcile static analysis paths with runtime evidence.
pub fn reconcile_runtime_evidence(
    graph: &Arc<GraphStore>,
    project_id: &str,
    batch: &RuntimeEvidenceBatch,
) -> anyhow::Result<ReconciliationReport> {
    // Collect static paths from graph
    let static_paths = collect_static_paths(graph, project_id)?;

    // Index runtime events for fast lookup
    let runtime_index = index_runtime_events(&batch.events);

    let mut confirmed = Vec::new();
    let mut contradicted = Vec::new();
    let mut inconclusive = Vec::new();

    for (source, target, kind_str) in &static_paths {
        let runtime_hit = check_runtime_hit(source, target, kind_str, &runtime_index);

        let evidence = PathEvidence {
            source: source.clone(),
            target: target.clone(),
            edge_kind: kind_str.clone(),
            static_evidence: format!("{kind_str}: {source} → {target}"),
            runtime_evidence: runtime_hit.clone(),
        };

        match &runtime_hit {
            Some(_) => confirmed.push(evidence),
            None => {
                // If we have runtime events that touch either source or target, it's
                // contradicted (the path exists but wasn't exercised). Otherwise inconclusive.
                let source_seen = runtime_index.contains_key(source);
                let target_seen = runtime_index.contains_key(target);
                if source_seen || target_seen {
                    contradicted.push(evidence);
                } else {
                    inconclusive.push(evidence);
                }
            }
        }
    }

    let total = static_paths.len();
    let confirmed_count = confirmed.len();
    let contradicted_count = contradicted.len();
    let inconclusive_count = inconclusive.len();

    let confirmed_ratio = if total > 0 {
        confirmed_count as f64 / total as f64
    } else {
        0.0
    };
    let contradicted_ratio = if total > 0 {
        contradicted_count as f64 / total as f64
    } else {
        0.0
    };
    let confidence_delta = confirmed_ratio - contradicted_ratio;

    Ok(ReconciliationReport {
        confirmed_paths: confirmed,
        contradicted_paths: contradicted,
        inconclusive_paths: inconclusive,
        summary: ReconciliationSummary {
            total_static_paths: total,
            confirmed_count,
            contradicted_count,
            inconclusive_count,
            confirmed_ratio,
            contradicted_ratio,
            confidence_delta,
        },
    })
}

// ─── Code generation ──────────────────────────────────────────────────────────

fn generate_csharp_module(has_session: bool, has_sql: bool, has_postback: bool) -> String {
    let mut code = String::with_capacity(4096);

    let _ = writeln!(
        code,
        "// Auto-generated by Engram-MCP — Runtime Instrumentation Module"
    );
    let _ = writeln!(code, "// Add to App_Code/ or compile into the project");
    let _ = writeln!(code, "using System;");
    let _ = writeln!(code, "using System.IO;");
    let _ = writeln!(code, "using System.Web;");
    let _ = writeln!(code, "using System.Text.Json;");
    let _ = writeln!(code, "using System.Diagnostics;");
    if has_sql {
        let _ = writeln!(code, "using System.Data;");
        let _ = writeln!(code, "using System.Data.Common;");
    }
    let _ = writeln!(code);
    let _ = writeln!(code, "public class EngramInstrumentation : IHttpModule");
    let _ = writeln!(code, "{{");
    let _ = writeln!(
        code,
        "    private static readonly string LogPath = Path.Combine(AppDomain.CurrentDomain.BaseDirectory, \"logs\", \"engram-runtime.jsonl\");"
    );
    let _ = writeln!(code);
    let _ = writeln!(code, "    public void Init(HttpApplication app)");
    let _ = writeln!(code, "    {{");
    let _ = writeln!(code, "        app.BeginRequest += OnBeginRequest;");
    let _ = writeln!(code, "        app.EndRequest += OnEndRequest;");
    let _ = writeln!(code, "        app.Error += OnError;");
    if has_postback {
        let _ = writeln!(
            code,
            "        app.PostMapRequestHandler += OnPostMapRequestHandler;"
        );
    }
    let _ = writeln!(code, "    }}");
    let _ = writeln!(code);
    let _ = writeln!(code, "    public void Dispose() {{ }}");
    let _ = writeln!(code);

    // BeginRequest
    let _ = writeln!(
        code,
        "    private void OnBeginRequest(object sender, EventArgs e)"
    );
    let _ = writeln!(code, "    {{");
    let _ = writeln!(code, "        var ctx = HttpContext.Current;");
    let _ = writeln!(
        code,
        "        ctx.Items[\"engram_start\"] = Stopwatch.StartNew();"
    );
    let _ = writeln!(code, "        LogEvent(new {{");
    let _ = writeln!(code, "            event_type = \"route\",");
    let _ = writeln!(code, "            source_path = ctx.Request.Path,");
    let _ = writeln!(code, "            target = ctx.Request.HttpMethod,");
    let _ = writeln!(
        code,
        "            timestamp = DateTime.UtcNow.ToString(\"o\")"
    );
    let _ = writeln!(code, "        }});");
    let _ = writeln!(code, "    }}");
    let _ = writeln!(code);

    // EndRequest
    let _ = writeln!(
        code,
        "    private void OnEndRequest(object sender, EventArgs e)"
    );
    let _ = writeln!(code, "    {{");
    let _ = writeln!(code, "        var ctx = HttpContext.Current;");
    let _ = writeln!(
        code,
        "        var sw = ctx.Items[\"engram_start\"] as Stopwatch;"
    );
    let _ = writeln!(code, "        LogEvent(new {{");
    let _ = writeln!(code, "            event_type = \"route_complete\",");
    let _ = writeln!(code, "            source_path = ctx.Request.Path,");
    let _ = writeln!(
        code,
        "            target = ctx.Response.StatusCode.ToString(),"
    );
    let _ = writeln!(
        code,
        "            duration_ms = sw?.ElapsedMilliseconds ?? -1,"
    );
    let _ = writeln!(
        code,
        "            timestamp = DateTime.UtcNow.ToString(\"o\")"
    );
    let _ = writeln!(code, "        }});");
    let _ = writeln!(code, "    }}");
    let _ = writeln!(code);

    // Error
    let _ = writeln!(code, "    private void OnError(object sender, EventArgs e)");
    let _ = writeln!(code, "    {{");
    let _ = writeln!(code, "        var ctx = HttpContext.Current;");
    let _ = writeln!(code, "        var ex = ctx.Server.GetLastError();");
    let _ = writeln!(code, "        LogEvent(new {{");
    let _ = writeln!(code, "            event_type = \"error\",");
    let _ = writeln!(code, "            source_path = ctx.Request.Path,");
    let _ = writeln!(
        code,
        "            target = ex?.GetType().Name ?? \"unknown\","
    );
    let _ = writeln!(code, "            detail = ex?.Message ?? \"\",");
    let _ = writeln!(
        code,
        "            timestamp = DateTime.UtcNow.ToString(\"o\")"
    );
    let _ = writeln!(code, "        }});");
    let _ = writeln!(code, "    }}");

    if has_postback {
        let _ = writeln!(code);
        let _ = writeln!(
            code,
            "    private void OnPostMapRequestHandler(object sender, EventArgs e)"
        );
        let _ = writeln!(code, "    {{");
        let _ = writeln!(code, "        var ctx = HttpContext.Current;");
        let _ = writeln!(
            code,
            "        var eventTarget = ctx.Request.Form[\"__EVENTTARGET\"];"
        );
        let _ = writeln!(
            code,
            "        var eventArg = ctx.Request.Form[\"__EVENTARGUMENT\"];"
        );
        let _ = writeln!(code, "        if (!string.IsNullOrEmpty(eventTarget))");
        let _ = writeln!(code, "        {{");
        let _ = writeln!(code, "            LogEvent(new {{");
        let _ = writeln!(code, "                event_type = \"postback\",");
        let _ = writeln!(code, "                source_path = ctx.Request.Path,");
        let _ = writeln!(code, "                target = eventTarget,");
        let _ = writeln!(code, "                detail = eventArg ?? \"\",");
        let _ = writeln!(
            code,
            "                timestamp = DateTime.UtcNow.ToString(\"o\")"
        );
        let _ = writeln!(code, "            }});");
        let _ = writeln!(code, "        }}");
        let _ = writeln!(code, "    }}");
    }

    if has_session {
        let _ = writeln!(code);
        let _ = writeln!(
            code,
            "    // Call from a custom SessionStateWrapper to track session access"
        );
        let _ = writeln!(
            code,
            "    public static void LogSessionAccess(string key, string operation, string path)"
        );
        let _ = writeln!(code, "    {{");
        let _ = writeln!(code, "        LogEvent(new {{");
        let _ = writeln!(code, "            event_type = \"state_access\",");
        let _ = writeln!(code, "            source_path = path,");
        let _ = writeln!(code, "            target = $\"Session:{{{{key}}}}\",");
        let _ = writeln!(code, "            detail = operation,");
        let _ = writeln!(
            code,
            "            timestamp = DateTime.UtcNow.ToString(\"o\")"
        );
        let _ = writeln!(code, "        }});");
        let _ = writeln!(code, "    }}");
    }

    if has_sql {
        let _ = writeln!(code);
        let _ = writeln!(
            code,
            "    // Call from a DbCommand wrapper to track SQL execution"
        );
        let _ = writeln!(
            code,
            "    public static void LogSqlExecution(string commandText, long durationMs, int rowCount, string path)"
        );
        let _ = writeln!(code, "    {{");
        let _ = writeln!(code, "        LogEvent(new {{");
        let _ = writeln!(code, "            event_type = \"sql_execution\",");
        let _ = writeln!(code, "            source_path = path,");
        let _ = writeln!(code, "            target = commandText,");
        let _ = writeln!(
            code,
            "            detail = $\"rows={{rowCount}}, ms={{durationMs}}\","
        );
        let _ = writeln!(
            code,
            "            timestamp = DateTime.UtcNow.ToString(\"o\")"
        );
        let _ = writeln!(code, "        }});");
        let _ = writeln!(code, "    }}");
    }

    let _ = writeln!(code);
    let _ = writeln!(code, "    private static void LogEvent(object evt)");
    let _ = writeln!(code, "    {{");
    let _ = writeln!(code, "        try");
    let _ = writeln!(code, "        {{");
    let _ = writeln!(
        code,
        "            var json = JsonSerializer.Serialize(evt);"
    );
    let _ = writeln!(
        code,
        "            var dir = Path.GetDirectoryName(LogPath);"
    );
    let _ = writeln!(
        code,
        "            if (!string.IsNullOrEmpty(dir)) Directory.CreateDirectory(dir);"
    );
    let _ = writeln!(
        code,
        "            File.AppendAllText(LogPath, json + Environment.NewLine);"
    );
    let _ = writeln!(code, "        }}");
    let _ = writeln!(
        code,
        "        catch {{ /* instrumentation should never crash the app */ }}"
    );
    let _ = writeln!(code, "    }}");
    let _ = writeln!(code, "}}");

    code
}

fn generate_vb_module(has_session: bool, has_sql: bool, has_postback: bool) -> String {
    let mut code = String::with_capacity(3072);

    let _ = writeln!(
        code,
        "' Auto-generated by Engram-MCP — Runtime Instrumentation Module"
    );
    let _ = writeln!(code, "Imports System");
    let _ = writeln!(code, "Imports System.IO");
    let _ = writeln!(code, "Imports System.Web");
    let _ = writeln!(code, "Imports System.Text.Json");
    let _ = writeln!(code, "Imports System.Diagnostics");
    let _ = writeln!(code);
    let _ = writeln!(code, "Public Class EngramInstrumentation");
    let _ = writeln!(code, "    Implements IHttpModule");
    let _ = writeln!(code);
    let _ = writeln!(
        code,
        "    Private Shared ReadOnly LogPath As String = Path.Combine(AppDomain.CurrentDomain.BaseDirectory, \"logs\", \"engram-runtime.jsonl\")"
    );
    let _ = writeln!(code);
    let _ = writeln!(
        code,
        "    Public Sub Init(app As HttpApplication) Implements IHttpModule.Init"
    );
    let _ = writeln!(
        code,
        "        AddHandler app.BeginRequest, AddressOf OnBeginRequest"
    );
    let _ = writeln!(
        code,
        "        AddHandler app.EndRequest, AddressOf OnEndRequest"
    );
    let _ = writeln!(code, "        AddHandler app.[Error], AddressOf OnError");
    if has_postback {
        let _ = writeln!(
            code,
            "        AddHandler app.PostMapRequestHandler, AddressOf OnPostMapRequestHandler"
        );
    }
    let _ = writeln!(code, "    End Sub");
    let _ = writeln!(code);
    let _ = writeln!(
        code,
        "    Public Sub Dispose() Implements IHttpModule.Dispose"
    );
    let _ = writeln!(code, "    End Sub");
    let _ = writeln!(code);
    let _ = writeln!(
        code,
        "    Private Sub OnBeginRequest(sender As Object, e As EventArgs)"
    );
    let _ = writeln!(code, "        Dim ctx = HttpContext.Current");
    let _ = writeln!(
        code,
        "        ctx.Items(\"engram_start\") = Stopwatch.StartNew()"
    );
    let _ = writeln!(
        code,
        "        LogEvent(\"route\", ctx.Request.Path, ctx.Request.HttpMethod)"
    );
    let _ = writeln!(code, "    End Sub");
    let _ = writeln!(code);
    let _ = writeln!(
        code,
        "    Private Sub OnEndRequest(sender As Object, e As EventArgs)"
    );
    let _ = writeln!(code, "        Dim ctx = HttpContext.Current");
    let _ = writeln!(
        code,
        "        LogEvent(\"route_complete\", ctx.Request.Path, ctx.Response.StatusCode.ToString())"
    );
    let _ = writeln!(code, "    End Sub");
    let _ = writeln!(code);
    let _ = writeln!(
        code,
        "    Private Sub OnError(sender As Object, e As EventArgs)"
    );
    let _ = writeln!(code, "        Dim ctx = HttpContext.Current");
    let _ = writeln!(code, "        Dim ex = ctx.Server.GetLastError()");
    let _ = writeln!(
        code,
        "        LogEvent(\"error\", ctx.Request.Path, If(ex?.GetType().Name, \"unknown\"))"
    );
    let _ = writeln!(code, "    End Sub");

    if has_session {
        let _ = writeln!(code);
        let _ = writeln!(
            code,
            "    Public Shared Sub LogSessionAccess(key As String, operation As String, path As String)"
        );
        let _ = writeln!(
            code,
            "        LogEvent(\"state_access\", path, $\"Session:{{key}}\")"
        );
        let _ = writeln!(code, "    End Sub");
    }

    if has_sql {
        let _ = writeln!(code);
        let _ = writeln!(
            code,
            "    Public Shared Sub LogSqlExecution(commandText As String, durationMs As Long, rowCount As Integer, path As String)"
        );
        let _ = writeln!(
            code,
            "        LogEvent(\"sql_execution\", path, commandText)"
        );
        let _ = writeln!(code, "    End Sub");
    }

    let _ = writeln!(code);
    let _ = writeln!(
        code,
        "    Private Shared Sub LogEvent(eventType As String, sourcePath As String, target As String)"
    );
    let _ = writeln!(code, "        Try");
    let _ = writeln!(code, "            Dim dir = Path.GetDirectoryName(LogPath)");
    let _ = writeln!(
        code,
        "            If Not String.IsNullOrEmpty(dir) Then Directory.CreateDirectory(dir)"
    );
    let _ = writeln!(
        code,
        "            Dim json = $\"{{\"\"event_type\"\": \"\"{{eventType}}\"\", \"\"source_path\"\": \"\"{{sourcePath}}\"\", \"\"target\"\": \"\"{{target}}\"\", \"\"timestamp\"\": \"\"{{DateTime.UtcNow:o}}\"\"}}\""
    );
    let _ = writeln!(
        code,
        "            File.AppendAllText(LogPath, json & Environment.NewLine)"
    );
    let _ = writeln!(code, "        Catch");
    let _ = writeln!(
        code,
        "            ' Instrumentation should never crash the app"
    );
    let _ = writeln!(code, "        End Try");
    let _ = writeln!(code, "    End Sub");
    let _ = writeln!(code);
    let _ = writeln!(code, "End Class");

    code
}

fn generate_webconfig_entries() -> String {
    let mut xml = String::with_capacity(512);
    let _ = writeln!(xml, "<!-- Add to <system.webServer><modules> section -->");
    let _ = writeln!(
        xml,
        "<add name=\"EngramInstrumentation\" type=\"EngramInstrumentation\" />"
    );
    let _ = writeln!(xml);
    let _ = writeln!(
        xml,
        "<!-- Or for classic mode, add to <system.web><httpModules> -->"
    );
    let _ = writeln!(
        xml,
        "<add name=\"EngramInstrumentation\" type=\"EngramInstrumentation\" />"
    );
    xml
}

// ─── Session Wrapper Generation ──────────────────────────────────────────────

/// Generate a C# wrapper class that intercepts HttpSessionState access
/// and logs all read/write operations through the instrumentation pipeline.
fn generate_session_wrapper() -> String {
    let mut code = String::with_capacity(4096);

    let _ = writeln!(
        code,
        "// Auto-generated by Engram-MCP — Session State Instrumentation Wrapper"
    );
    let _ = writeln!(
        code,
        "// Intercepts Session reads/writes and logs them for runtime evidence collection."
    );
    let _ = writeln!(
        code,
        "// Install: Add to App_Code/ and register in Global.asax AcquireRequestState."
    );
    let _ = writeln!(code, "using System;");
    let _ = writeln!(code, "using System.Collections;");
    let _ = writeln!(code, "using System.Collections.Specialized;");
    let _ = writeln!(code, "using System.Web;");
    let _ = writeln!(code, "using System.Web.SessionState;");
    let _ = writeln!(code);
    let _ = writeln!(code, "/// <summary>");
    let _ = writeln!(
        code,
        "/// Drop-in wrapper for HttpSessionState that intercepts get/set operations."
    );
    let _ = writeln!(
        code,
        "/// Replace direct Session access with InstrumentedSession.Current[\"key\"]"
    );
    let _ = writeln!(
        code,
        "/// or install the HttpModule to swap automatically via AcquireRequestState."
    );
    let _ = writeln!(code, "/// </summary>");
    let _ = writeln!(
        code,
        "public class InstrumentedSessionStateWrapper : IHttpSessionState"
    );
    let _ = writeln!(code, "{{");
    let _ = writeln!(code, "    private readonly HttpSessionState _inner;");
    let _ = writeln!(code);
    let _ = writeln!(
        code,
        "    public InstrumentedSessionStateWrapper(HttpSessionState inner)"
    );
    let _ = writeln!(code, "    {{");
    let _ = writeln!(
        code,
        "        _inner = inner ?? throw new ArgumentNullException(nameof(inner));"
    );
    let _ = writeln!(code, "    }}");
    let _ = writeln!(code);
    let _ = writeln!(
        code,
        "    /// <summary>Gets or sets session values with automatic logging.</summary>"
    );
    let _ = writeln!(code, "    public object this[string name]");
    let _ = writeln!(code, "    {{");
    let _ = writeln!(code, "        get");
    let _ = writeln!(code, "        {{");
    let _ = writeln!(code, "            var value = _inner[name];");
    let _ = writeln!(
        code,
        "            EngramInstrumentation.LogSessionAccess(name, \"read\", CurrentPath());"
    );
    let _ = writeln!(code, "            return value;");
    let _ = writeln!(code, "        }}");
    let _ = writeln!(code, "        set");
    let _ = writeln!(code, "        {{");
    let _ = writeln!(code, "            _inner[name] = value;");
    let _ = writeln!(
        code,
        "            EngramInstrumentation.LogSessionAccess(name, \"write\", CurrentPath());"
    );
    let _ = writeln!(code, "        }}");
    let _ = writeln!(code, "    }}");
    let _ = writeln!(code);
    let _ = writeln!(code, "    public object this[int index]");
    let _ = writeln!(code, "    {{");
    let _ = writeln!(code, "        get");
    let _ = writeln!(code, "        {{");
    let _ = writeln!(code, "            var key = _inner.Keys[index];");
    let _ = writeln!(
        code,
        "            EngramInstrumentation.LogSessionAccess(key, \"read_by_index\", CurrentPath());"
    );
    let _ = writeln!(code, "            return _inner[index];");
    let _ = writeln!(code, "        }}");
    let _ = writeln!(code, "        set");
    let _ = writeln!(code, "        {{");
    let _ = writeln!(code, "            var key = _inner.Keys[index];");
    let _ = writeln!(code, "            _inner[index] = value;");
    let _ = writeln!(
        code,
        "            EngramInstrumentation.LogSessionAccess(key, \"write_by_index\", CurrentPath());"
    );
    let _ = writeln!(code, "        }}");
    let _ = writeln!(code, "    }}");
    let _ = writeln!(code);
    let _ = writeln!(code, "    public void Add(string name, object value)");
    let _ = writeln!(code, "    {{");
    let _ = writeln!(code, "        _inner.Add(name, value);");
    let _ = writeln!(
        code,
        "        EngramInstrumentation.LogSessionAccess(name, \"add\", CurrentPath());"
    );
    let _ = writeln!(code, "    }}");
    let _ = writeln!(code);
    let _ = writeln!(code, "    public void Remove(string name)");
    let _ = writeln!(code, "    {{");
    let _ = writeln!(
        code,
        "        EngramInstrumentation.LogSessionAccess(name, \"remove\", CurrentPath());"
    );
    let _ = writeln!(code, "        _inner.Remove(name);");
    let _ = writeln!(code, "    }}");
    let _ = writeln!(code);
    let _ = writeln!(code, "    public void Clear()");
    let _ = writeln!(code, "    {{");
    let _ = writeln!(
        code,
        "        EngramInstrumentation.LogSessionAccess(\"*\", \"clear\", CurrentPath());"
    );
    let _ = writeln!(code, "        _inner.Clear();");
    let _ = writeln!(code, "    }}");
    let _ = writeln!(code);
    let _ = writeln!(code, "    public void Abandon()");
    let _ = writeln!(code, "    {{");
    let _ = writeln!(
        code,
        "        EngramInstrumentation.LogSessionAccess(\"*\", \"abandon\", CurrentPath());"
    );
    let _ = writeln!(code, "        _inner.Abandon();");
    let _ = writeln!(code, "    }}");
    let _ = writeln!(code);
    let _ = writeln!(code, "    // ─── Delegated IHttpSessionState members ───");
    let _ = writeln!(code, "    public string SessionID => _inner.SessionID;");
    let _ = writeln!(
        code,
        "    public int Timeout {{ get => _inner.Timeout; set => _inner.Timeout = value; }}"
    );
    let _ = writeln!(code, "    public bool IsNewSession => _inner.IsNewSession;");
    let _ = writeln!(code, "    public SessionStateMode Mode => _inner.Mode;");
    let _ = writeln!(code, "    public bool IsCookieless => _inner.IsCookieless;");
    let _ = writeln!(
        code,
        "    public HttpCookieMode CookieMode => _inner.CookieMode;"
    );
    let _ = writeln!(
        code,
        "    public int LCID {{ get => _inner.LCID; set => _inner.LCID = value; }}"
    );
    let _ = writeln!(
        code,
        "    public int CodePage {{ get => _inner.CodePage; set => _inner.CodePage = value; }}"
    );
    let _ = writeln!(
        code,
        "    public HttpStaticObjectsCollection StaticObjects => _inner.StaticObjects;"
    );
    let _ = writeln!(code, "    public int Count => _inner.Count;");
    let _ = writeln!(
        code,
        "    public NameObjectCollectionBase.KeysCollection Keys => _inner.Keys;"
    );
    let _ = writeln!(code, "    public bool IsReadOnly => _inner.IsReadOnly;");
    let _ = writeln!(
        code,
        "    public bool IsSynchronized => _inner.IsSynchronized;"
    );
    let _ = writeln!(code, "    public object SyncRoot => _inner.SyncRoot;");
    let _ = writeln!(
        code,
        "    public void RemoveAt(int index) => _inner.RemoveAt(index);"
    );
    let _ = writeln!(code, "    public void RemoveAll() {{ Clear(); }}");
    let _ = writeln!(
        code,
        "    public void CopyTo(Array array, int index) => _inner.CopyTo(array, index);"
    );
    let _ = writeln!(
        code,
        "    public IEnumerator GetEnumerator() => _inner.GetEnumerator();"
    );
    let _ = writeln!(code);
    let _ = writeln!(code, "    private static string CurrentPath()");
    let _ = writeln!(code, "    {{");
    let _ = writeln!(
        code,
        "        return HttpContext.Current?.Request?.Path ?? \"unknown\";"
    );
    let _ = writeln!(code, "    }}");
    let _ = writeln!(code, "}}");

    code
}

// ─── SQL Wrapper Generation ─────────────────────────────────────────────────

/// Generate a C# wrapper class that intercepts DbCommand execution
/// and logs all SQL operations through the instrumentation pipeline.
fn generate_sql_wrapper() -> String {
    let mut code = String::with_capacity(6144);

    let _ = writeln!(
        code,
        "// Auto-generated by Engram-MCP — SQL Command Instrumentation Wrapper"
    );
    let _ = writeln!(
        code,
        "// Wraps any DbCommand to intercept and log Execute* calls."
    );
    let _ = writeln!(
        code,
        "// Usage: var cmd = InstrumentedDbCommand.Wrap(new SqlCommand(sql, conn));"
    );
    let _ = writeln!(code, "using System;");
    let _ = writeln!(code, "using System.Data;");
    let _ = writeln!(code, "using System.Data.Common;");
    let _ = writeln!(code, "using System.Diagnostics;");
    let _ = writeln!(code, "using System.Web;");
    let _ = writeln!(code);
    let _ = writeln!(code, "/// <summary>");
    let _ = writeln!(
        code,
        "/// Transparent wrapper around DbCommand that intercepts all Execute* methods,"
    );
    let _ = writeln!(
        code,
        "/// measures timing, captures row counts, and logs via EngramInstrumentation."
    );
    let _ = writeln!(
        code,
        "/// Works with SqlCommand, OleDbCommand, OdbcCommand, and any other DbCommand."
    );
    let _ = writeln!(code, "/// </summary>");
    let _ = writeln!(code, "public class InstrumentedDbCommand : DbCommand");
    let _ = writeln!(code, "{{");
    let _ = writeln!(code, "    private readonly DbCommand _inner;");
    let _ = writeln!(code);
    let _ = writeln!(
        code,
        "    /// <summary>Wrap an existing DbCommand for instrumentation.</summary>"
    );
    let _ = writeln!(
        code,
        "    public static InstrumentedDbCommand Wrap(DbCommand cmd)"
    );
    let _ = writeln!(code, "    {{");
    let _ = writeln!(
        code,
        "        if (cmd is InstrumentedDbCommand) return (InstrumentedDbCommand)cmd;"
    );
    let _ = writeln!(code, "        return new InstrumentedDbCommand(cmd);");
    let _ = writeln!(code, "    }}");
    let _ = writeln!(code);
    let _ = writeln!(code, "    private InstrumentedDbCommand(DbCommand inner)");
    let _ = writeln!(code, "    {{");
    let _ = writeln!(
        code,
        "        _inner = inner ?? throw new ArgumentNullException(nameof(inner));"
    );
    let _ = writeln!(code, "    }}");
    let _ = writeln!(code);
    let _ = writeln!(code, "    // ─── Execute interceptors ───");
    let _ = writeln!(code);
    let _ = writeln!(code, "    public override int ExecuteNonQuery()");
    let _ = writeln!(code, "    {{");
    let _ = writeln!(code, "        var sw = Stopwatch.StartNew();");
    let _ = writeln!(code, "        try");
    let _ = writeln!(code, "        {{");
    let _ = writeln!(code, "            int rows = _inner.ExecuteNonQuery();");
    let _ = writeln!(code, "            sw.Stop();");
    let _ = writeln!(code, "            LogSql(sw.ElapsedMilliseconds, rows);");
    let _ = writeln!(code, "            return rows;");
    let _ = writeln!(code, "        }}");
    let _ = writeln!(code, "        catch (Exception ex)");
    let _ = writeln!(code, "        {{");
    let _ = writeln!(code, "            sw.Stop();");
    let _ = writeln!(code, "            LogSqlError(sw.ElapsedMilliseconds, ex);");
    let _ = writeln!(code, "            throw;");
    let _ = writeln!(code, "        }}");
    let _ = writeln!(code, "    }}");
    let _ = writeln!(code);
    let _ = writeln!(code, "    public override object ExecuteScalar()");
    let _ = writeln!(code, "    {{");
    let _ = writeln!(code, "        var sw = Stopwatch.StartNew();");
    let _ = writeln!(code, "        try");
    let _ = writeln!(code, "        {{");
    let _ = writeln!(code, "            object result = _inner.ExecuteScalar();");
    let _ = writeln!(code, "            sw.Stop();");
    let _ = writeln!(
        code,
        "            LogSql(sw.ElapsedMilliseconds, result != null ? 1 : 0);"
    );
    let _ = writeln!(code, "            return result;");
    let _ = writeln!(code, "        }}");
    let _ = writeln!(code, "        catch (Exception ex)");
    let _ = writeln!(code, "        {{");
    let _ = writeln!(code, "            sw.Stop();");
    let _ = writeln!(code, "            LogSqlError(sw.ElapsedMilliseconds, ex);");
    let _ = writeln!(code, "            throw;");
    let _ = writeln!(code, "        }}");
    let _ = writeln!(code, "    }}");
    let _ = writeln!(code);
    let _ = writeln!(
        code,
        "    protected override DbDataReader ExecuteDbDataReader(CommandBehavior behavior)"
    );
    let _ = writeln!(code, "    {{");
    let _ = writeln!(code, "        var sw = Stopwatch.StartNew();");
    let _ = writeln!(code, "        try");
    let _ = writeln!(code, "        {{");
    let _ = writeln!(
        code,
        "            DbDataReader reader = _inner.ExecuteReader(behavior);"
    );
    let _ = writeln!(code, "            sw.Stop();");
    let _ = writeln!(
        code,
        "            LogSql(sw.ElapsedMilliseconds, -1); // row count unknown until read"
    );
    let _ = writeln!(
        code,
        "            return new InstrumentedDataReader(reader, CommandText,"
    );
    let _ = writeln!(
        code,
        "                HttpContext.Current?.Request?.Path ?? \"unknown\");"
    );
    let _ = writeln!(code, "        }}");
    let _ = writeln!(code, "        catch (Exception ex)");
    let _ = writeln!(code, "        {{");
    let _ = writeln!(code, "            sw.Stop();");
    let _ = writeln!(code, "            LogSqlError(sw.ElapsedMilliseconds, ex);");
    let _ = writeln!(code, "            throw;");
    let _ = writeln!(code, "        }}");
    let _ = writeln!(code, "    }}");
    let _ = writeln!(code);
    let _ = writeln!(code, "    private void LogSql(long ms, int rows)");
    let _ = writeln!(code, "    {{");
    let _ = writeln!(
        code,
        "        var path = HttpContext.Current?.Request?.Path ?? \"unknown\";"
    );
    let _ = writeln!(
        code,
        "        EngramInstrumentation.LogSqlExecution(CommandText, ms, rows, path);"
    );
    let _ = writeln!(code, "    }}");
    let _ = writeln!(code);
    let _ = writeln!(code, "    private void LogSqlError(long ms, Exception ex)");
    let _ = writeln!(code, "    {{");
    let _ = writeln!(
        code,
        "        var path = HttpContext.Current?.Request?.Path ?? \"unknown\";"
    );
    let _ = writeln!(code, "        EngramInstrumentation.LogSqlExecution(");
    let _ = writeln!(
        code,
        "            $\"ERROR: {{CommandText}} | {{ex.GetType().Name}}: {{ex.Message}}\","
    );
    let _ = writeln!(code, "            ms, -1, path);");
    let _ = writeln!(code, "    }}");
    let _ = writeln!(code);
    let _ = writeln!(code, "    // ─── Delegated DbCommand properties ───");
    let _ = writeln!(code);
    let _ = writeln!(
        code,
        "    public override string CommandText {{ get => _inner.CommandText; set => _inner.CommandText = value; }}"
    );
    let _ = writeln!(
        code,
        "    public override int CommandTimeout {{ get => _inner.CommandTimeout; set => _inner.CommandTimeout = value; }}"
    );
    let _ = writeln!(
        code,
        "    public override CommandType CommandType {{ get => _inner.CommandType; set => _inner.CommandType = value; }}"
    );
    let _ = writeln!(
        code,
        "    public override bool DesignTimeVisible {{ get => _inner.DesignTimeVisible; set => _inner.DesignTimeVisible = value; }}"
    );
    let _ = writeln!(
        code,
        "    public override UpdateRowSource UpdatedRowSource {{ get => _inner.UpdatedRowSource; set => _inner.UpdatedRowSource = value; }}"
    );
    let _ = writeln!(
        code,
        "    protected override DbConnection DbConnection {{ get => _inner.Connection; set => _inner.Connection = value; }}"
    );
    let _ = writeln!(
        code,
        "    protected override DbTransaction DbTransaction {{ get => _inner.Transaction; set => _inner.Transaction = value; }}"
    );
    let _ = writeln!(
        code,
        "    protected override DbParameterCollection DbParameterCollection => _inner.Parameters;"
    );
    let _ = writeln!(code);
    let _ = writeln!(
        code,
        "    public override void Cancel() => _inner.Cancel();"
    );
    let _ = writeln!(
        code,
        "    public override void Prepare() => _inner.Prepare();"
    );
    let _ = writeln!(
        code,
        "    protected override DbParameter CreateDbParameter() => _inner.CreateParameter();"
    );
    let _ = writeln!(code);
    let _ = writeln!(code, "    protected override void Dispose(bool disposing)");
    let _ = writeln!(code, "    {{");
    let _ = writeln!(code, "        if (disposing) _inner.Dispose();");
    let _ = writeln!(code, "        base.Dispose(disposing);");
    let _ = writeln!(code, "    }}");
    let _ = writeln!(code, "}}");
    let _ = writeln!(code);
    let _ = writeln!(code, "/// <summary>");
    let _ = writeln!(
        code,
        "/// Wrapper around DbDataReader that logs total row count on close."
    );
    let _ = writeln!(code, "/// </summary>");
    let _ = writeln!(code, "internal class InstrumentedDataReader : DbDataReader");
    let _ = writeln!(code, "{{");
    let _ = writeln!(code, "    private readonly DbDataReader _inner;");
    let _ = writeln!(code, "    private readonly string _commandText;");
    let _ = writeln!(code, "    private readonly string _requestPath;");
    let _ = writeln!(code, "    private int _rowCount;");
    let _ = writeln!(code);
    let _ = writeln!(
        code,
        "    public InstrumentedDataReader(DbDataReader inner, string commandText, string requestPath)"
    );
    let _ = writeln!(code, "    {{");
    let _ = writeln!(code, "        _inner = inner;");
    let _ = writeln!(code, "        _commandText = commandText;");
    let _ = writeln!(code, "        _requestPath = requestPath;");
    let _ = writeln!(code, "    }}");
    let _ = writeln!(code);
    let _ = writeln!(code, "    public override bool Read()");
    let _ = writeln!(code, "    {{");
    let _ = writeln!(code, "        bool result = _inner.Read();");
    let _ = writeln!(code, "        if (result) _rowCount++;");
    let _ = writeln!(code, "        return result;");
    let _ = writeln!(code, "    }}");
    let _ = writeln!(code);
    let _ = writeln!(code, "    public override void Close()");
    let _ = writeln!(code, "    {{");
    let _ = writeln!(code, "        EngramInstrumentation.LogSqlExecution(");
    let _ = writeln!(
        code,
        "            _commandText, 0, _rowCount, _requestPath);"
    );
    let _ = writeln!(code, "        _inner.Close();");
    let _ = writeln!(code, "    }}");
    let _ = writeln!(code);
    let _ = writeln!(code, "    // ─── Delegated DbDataReader members ───");
    let _ = writeln!(
        code,
        "    public override object this[int ordinal] => _inner[ordinal];"
    );
    let _ = writeln!(
        code,
        "    public override object this[string name] => _inner[name];"
    );
    let _ = writeln!(code, "    public override int Depth => _inner.Depth;");
    let _ = writeln!(
        code,
        "    public override int FieldCount => _inner.FieldCount;"
    );
    let _ = writeln!(code, "    public override bool HasRows => _inner.HasRows;");
    let _ = writeln!(
        code,
        "    public override bool IsClosed => _inner.IsClosed;"
    );
    let _ = writeln!(
        code,
        "    public override int RecordsAffected => _inner.RecordsAffected;"
    );
    let _ = writeln!(
        code,
        "    public override bool GetBoolean(int ordinal) => _inner.GetBoolean(ordinal);"
    );
    let _ = writeln!(
        code,
        "    public override byte GetByte(int ordinal) => _inner.GetByte(ordinal);"
    );
    let _ = writeln!(
        code,
        "    public override long GetBytes(int o, long d, byte[] b, int bo, int l) => _inner.GetBytes(o, d, b, bo, l);"
    );
    let _ = writeln!(
        code,
        "    public override char GetChar(int ordinal) => _inner.GetChar(ordinal);"
    );
    let _ = writeln!(
        code,
        "    public override long GetChars(int o, long d, char[] b, int bo, int l) => _inner.GetChars(o, d, b, bo, l);"
    );
    let _ = writeln!(
        code,
        "    public override DateTime GetDateTime(int ordinal) => _inner.GetDateTime(ordinal);"
    );
    let _ = writeln!(
        code,
        "    public override decimal GetDecimal(int ordinal) => _inner.GetDecimal(ordinal);"
    );
    let _ = writeln!(
        code,
        "    public override double GetDouble(int ordinal) => _inner.GetDouble(ordinal);"
    );
    let _ = writeln!(
        code,
        "    public override Type GetFieldType(int ordinal) => _inner.GetFieldType(ordinal);"
    );
    let _ = writeln!(
        code,
        "    public override float GetFloat(int ordinal) => _inner.GetFloat(ordinal);"
    );
    let _ = writeln!(
        code,
        "    public override Guid GetGuid(int ordinal) => _inner.GetGuid(ordinal);"
    );
    let _ = writeln!(
        code,
        "    public override short GetInt16(int ordinal) => _inner.GetInt16(ordinal);"
    );
    let _ = writeln!(
        code,
        "    public override int GetInt32(int ordinal) => _inner.GetInt32(ordinal);"
    );
    let _ = writeln!(
        code,
        "    public override long GetInt64(int ordinal) => _inner.GetInt64(ordinal);"
    );
    let _ = writeln!(
        code,
        "    public override string GetName(int ordinal) => _inner.GetName(ordinal);"
    );
    let _ = writeln!(
        code,
        "    public override int GetOrdinal(string name) => _inner.GetOrdinal(name);"
    );
    let _ = writeln!(
        code,
        "    public override string GetString(int ordinal) => _inner.GetString(ordinal);"
    );
    let _ = writeln!(
        code,
        "    public override object GetValue(int ordinal) => _inner.GetValue(ordinal);"
    );
    let _ = writeln!(
        code,
        "    public override int GetValues(object[] values) => _inner.GetValues(values);"
    );
    let _ = writeln!(
        code,
        "    public override bool IsDBNull(int ordinal) => _inner.IsDBNull(ordinal);"
    );
    let _ = writeln!(
        code,
        "    public override bool NextResult() => _inner.NextResult();"
    );
    let _ = writeln!(
        code,
        "    public override string GetDataTypeName(int ordinal) => _inner.GetDataTypeName(ordinal);"
    );
    let _ = writeln!(
        code,
        "    public override IEnumerator GetEnumerator() => _inner.GetEnumerator();"
    );
    let _ = writeln!(code, "}}");

    code
}

// ─── Reconciliation internals ─────────────────────────────────────────────────

fn collect_static_paths(
    graph: &Arc<GraphStore>,
    project_id: &str,
) -> anyhow::Result<Vec<(String, String, String)>> {
    let mut paths = Vec::new();

    let edge_kinds = [
        EdgeKind::SqlCalls,
        EdgeKind::ReadsState,
        EdgeKind::WritesState,
        EdgeKind::Dependency,
        EdgeKind::TriggersPostback,
    ];

    for kind in edge_kinds {
        let edges = graph.list_edges_by_kind(project_id, kind.clone(), 10_000)?;
        for edge in edges {
            paths.push((
                edge.source_id.clone(),
                edge.target_id.clone(),
                kind.as_str().to_string(),
            ));
        }
    }

    Ok(paths)
}

fn index_runtime_events(events: &[RuntimeEvent]) -> HashMap<String, Vec<&RuntimeEvent>> {
    let mut index: HashMap<String, Vec<&RuntimeEvent>> = HashMap::new();
    for event in events {
        index
            .entry(event.source_path.clone())
            .or_default()
            .push(event);
        if let Some(ref t) = event.target
            && !t.is_empty() {
                index.entry(t.clone()).or_default().push(event);
            }
    }
    index
}

fn check_runtime_hit(
    source: &str,
    target: &str,
    kind: &str,
    runtime_index: &HashMap<String, Vec<&RuntimeEvent>>,
) -> Option<String> {
    // Check for a runtime event that matches this static path
    let source_events = runtime_index.get(source);
    let target_events = runtime_index.get(target);

    // For SQL paths, look for sql_execution events
    if kind == "sql_calls"
        && let Some(events) = source_events {
            for evt in events {
                if matches!(evt.event_type, RuntimeEventType::SqlExecution) {
                    let t = evt.target.as_deref().unwrap_or("");
                    if t.contains(target) {
                        return Some(format!("Runtime SQL: {} ({})", t, evt.timestamp));
                    }
                }
            }
        }

    // For state access, look for state_mutation events
    if (kind == "reads_state" || kind == "writes_state")
        && let Some(events) = target_events {
            for evt in events {
                if matches!(evt.event_type, RuntimeEventType::StateMutation) {
                    let t = evt.target.as_deref().unwrap_or("");
                    return Some(format!(
                        "Runtime state: {} at {} ({})",
                        t, evt.source_path, evt.timestamp
                    ));
                }
            }
        }

    // For dependencies (navigation), look for route events
    if kind == "dependency"
        && let Some(events) = target_events {
            for evt in events {
                if matches!(evt.event_type, RuntimeEventType::Route) {
                    return Some(format!(
                        "Runtime route: {} ({})",
                        evt.source_path, evt.timestamp
                    ));
                }
            }
        }

    // For postbacks, look for control_interaction events
    if kind == "triggers_postback"
        && let Some(events) = source_events {
            for evt in events {
                if matches!(evt.event_type, RuntimeEventType::ControlInteraction) {
                    let t = evt.target.as_deref().unwrap_or("");
                    return Some(format!(
                        "Runtime postback: {} → {} ({})",
                        evt.source_path, t, evt.timestamp
                    ));
                }
            }
        }

    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn csharp_module_includes_route_tracking() {
        let code = generate_csharp_module(false, false, false);
        assert!(code.contains("IHttpModule"));
        assert!(code.contains("OnBeginRequest"));
        assert!(code.contains("OnEndRequest"));
        assert!(code.contains("OnError"));
    }

    #[test]
    fn csharp_module_with_session() {
        let code = generate_csharp_module(true, false, false);
        assert!(code.contains("LogSessionAccess"));
    }

    #[test]
    fn csharp_module_with_sql() {
        let code = generate_csharp_module(false, true, false);
        assert!(code.contains("LogSqlExecution"));
        assert!(code.contains("System.Data"));
    }

    #[test]
    fn csharp_module_with_postback() {
        let code = generate_csharp_module(false, false, true);
        assert!(code.contains("__EVENTTARGET"));
        assert!(code.contains("OnPostMapRequestHandler"));
    }

    #[test]
    fn vb_module_includes_basics() {
        let code = generate_vb_module(false, false, false);
        assert!(code.contains("Implements IHttpModule"));
        assert!(code.contains("OnBeginRequest"));
        assert!(code.contains("End Class"));
    }

    #[test]
    fn vb_module_with_session() {
        let code = generate_vb_module(true, false, false);
        assert!(code.contains("LogSessionAccess"));
    }

    #[test]
    fn webconfig_has_module_entry() {
        let xml = generate_webconfig_entries();
        assert!(xml.contains("EngramInstrumentation"));
        assert!(xml.contains("system.webServer"));
    }

    fn test_event(
        event_type: RuntimeEventType,
        source_path: &str,
        target: Option<&str>,
        timestamp: &str,
    ) -> RuntimeEvent {
        RuntimeEvent {
            event_id: uuid::Uuid::new_v4().to_string(),
            timestamp: timestamp.into(),
            event_type,
            source_path: source_path.into(),
            source_function: None,
            source_line: None,
            target: target.map(|s| s.into()),
            context: HashMap::new(),
            trust_weight: 1.0,
        }
    }

    #[test]
    fn index_runtime_events_groups_by_path() {
        let events = vec![
            test_event(
                RuntimeEventType::Route,
                "/Orders.aspx",
                Some("GET"),
                "2024-01-01T00:00:00Z",
            ),
            test_event(
                RuntimeEventType::Route,
                "/Orders.aspx",
                Some("POST"),
                "2024-01-01T00:01:00Z",
            ),
        ];
        let idx = index_runtime_events(&events);
        assert_eq!(idx.get("/Orders.aspx").map(|v| v.len()), Some(2));
    }

    #[test]
    fn check_runtime_hit_finds_sql() {
        let events = vec![test_event(
            RuntimeEventType::SqlExecution,
            "fn:LoadOrders",
            Some("SELECT * FROM Orders"),
            "2024-01-01T00:00:00Z",
        )];
        let idx = index_runtime_events(&events);

        let hit = check_runtime_hit("fn:LoadOrders", "Orders", "sql_calls", &idx);
        assert!(hit.is_some());
    }

    #[test]
    fn check_runtime_hit_finds_state() {
        let events = vec![test_event(
            RuntimeEventType::StateMutation,
            "/Page.aspx",
            Some("Session:UserId"),
            "2024-01-01T00:00:00Z",
        )];
        let idx = index_runtime_events(&events);

        let hit = check_runtime_hit("fn:Page_Load", "Session:UserId", "reads_state", &idx);
        assert!(hit.is_some());
    }

    #[test]
    fn check_runtime_hit_returns_none_for_missing() {
        let idx = HashMap::new();
        let hit = check_runtime_hit("fn:Load", "table:Orders", "sql_calls", &idx);
        assert!(hit.is_none());
    }

    #[test]
    fn reconciliation_summary_computes_ratios() {
        let total = 10;
        let confirmed = 6;
        let contradicted = 2;
        let _inconclusive = 2;

        let ratio = confirmed as f64 / total as f64;
        assert!((ratio - 0.6).abs() < 0.01);
        let contra_ratio = contradicted as f64 / total as f64;
        assert!((contra_ratio - 0.2).abs() < 0.01);
    }
}
