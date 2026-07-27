//! Characterization test generator — produces test skeletons from extraction data.
//!
//! Analyzes event handlers, data flows, state transitions, and navigation paths
//! from graph edges and generates test classes covering each extraction edge.

use engram_graph::{Edge, EdgeKind, GraphStore, Node};
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write;
use std::sync::Arc;

/// Result of characterization test generation.
#[derive(Debug, Clone, Serialize)]
pub struct CharacterizationTestResult {
    /// Generated test class code.
    pub test_code: String,
    /// Coverage map: which extraction edges each test covers.
    pub coverage_map: Vec<TestCoverageEntry>,
    /// Test framework used.
    pub framework: String,
    /// Number of tests generated.
    pub test_count: usize,
    /// Warnings during generation.
    pub warnings: Vec<String>,
}

/// Coverage entry mapping a test to the edges it covers.
#[derive(Debug, Clone, Serialize)]
pub struct TestCoverageEntry {
    pub test_name: String,
    pub category: TestCategory,
    pub covered_edges: Vec<String>,
    pub covered_nodes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TestCategory {
    EventHandler,
    DataFlow,
    StateTransition,
    Navigation,
    ApiContract,
}

/// Test framework selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestFramework {
    NUnit,
    XUnit,
    MSTest,
}

impl TestFramework {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s.to_lowercase().as_str() {
            "nunit" | "n-unit" => Ok(Self::NUnit),
            "xunit" | "x-unit" => Ok(Self::XUnit),
            "mstest" | "ms-test" | "ms_test" => Ok(Self::MSTest),
            other => Err(format!(
                "unknown test framework '{}': must be one of nunit, xunit, mstest",
                other
            )),
        }
    }

    fn test_attribute(&self) -> &'static str {
        match self {
            Self::NUnit => "[Test]",
            Self::XUnit => "[Fact]",
            Self::MSTest => "[TestMethod]",
        }
    }

    fn fixture_attribute(&self) -> &'static str {
        match self {
            Self::NUnit => "[TestFixture]",
            Self::XUnit => "",
            Self::MSTest => "[TestClass]",
        }
    }

    fn setup_attribute(&self) -> &'static str {
        match self {
            Self::NUnit => "[SetUp]",
            Self::XUnit => "",
            Self::MSTest => "[TestInitialize]",
        }
    }

    fn assert_eq(&self, left: &str, right: &str) -> String {
        match self {
            Self::NUnit => format!("Assert.That({left}, Is.EqualTo({right}));"),
            Self::XUnit => format!("Assert.Equal({right}, {left});"),
            Self::MSTest => format!("Assert.AreEqual({right}, {left});"),
        }
    }

    fn assert_not_null(&self, expr: &str) -> String {
        match self {
            Self::NUnit => format!("Assert.That({expr}, Is.Not.Null);"),
            Self::XUnit => format!("Assert.NotNull({expr});"),
            Self::MSTest => format!("Assert.IsNotNull({expr});"),
        }
    }

    fn usings(&self) -> &'static str {
        match self {
            Self::NUnit => "using NUnit.Framework;\nusing Moq;",
            Self::XUnit => "using Xunit;\nusing Moq;",
            Self::MSTest => "using Microsoft.VisualStudio.TestTools.UnitTesting;\nusing Moq;",
        }
    }
}

/// Context collected from graph for test generation.
#[allow(dead_code)]
struct TestGenContext {
    functions: Vec<Node>,
    sql_edges: Vec<Edge>,
    reads_state: Vec<Edge>,
    writes_state: Vec<Edge>,
    reads_column: Vec<Edge>,
    queries_table: Vec<Edge>,
    parameter_bindings: Vec<Edge>,
    triggers_postback: Vec<Edge>,
    dependency_edges: Vec<Edge>,
    service_edges: Vec<Edge>,
    connection_strings: Vec<Node>,
}

// ─── Public API ───────────────────────────────────────────────────────────────

/// Generate characterization tests for a specific file.
pub fn generate_characterization_tests(
    graph: &Arc<GraphStore>,
    project_id: &str,
    file_path: &str,
    framework_str: &str,
) -> anyhow::Result<CharacterizationTestResult> {
    let fw = TestFramework::from_str(framework_str).map_err(|e| anyhow::anyhow!(e))?;
    let ctx = collect_test_context(graph, project_id, file_path)?;
    let mut warnings = Vec::new();

    let page_name = extract_page_name(file_path);
    let class_name = format!("{page_name}_CharacterizationTests");

    let mut code = String::with_capacity(8192);
    let mut coverage = Vec::new();
    let mut test_count = 0;

    // Header
    let _ = writeln!(code, "{}", fw.usings());
    let _ = writeln!(code, "using System.Threading.Tasks;");
    let _ = writeln!(code, "using System.Collections.Generic;");
    let _ = writeln!(code, "using System.Data;");
    let _ = writeln!(code, "using System.Web;");
    let _ = writeln!(code, "using System.Web.SessionState;");
    let _ = writeln!(code);
    let _ = writeln!(
        code,
        "// Auto-generated characterization tests for {file_path}"
    );
    let _ = writeln!(
        code,
        "// Generated by Engram-MCP — capture current behavior before migration"
    );
    let _ = writeln!(code);

    // Test infrastructure helper classes
    let _ = writeln!(code, "// ─── Test Infrastructure ───");
    let _ = writeln!(
        code,
        "// The classes below enable executable characterization tests."
    );
    let _ = writeln!(
        code,
        "// Adapt the connection string and page creation for your specific project."
    );
    let _ = writeln!(code);
    emit_helper_classes(&mut code, &page_name);
    let _ = writeln!(code);

    if !fw.fixture_attribute().is_empty() {
        let _ = writeln!(code, "{}", fw.fixture_attribute());
    }
    let _ = writeln!(code, "public class {class_name}");
    let _ = writeln!(code, "{{");

    // Fields
    let _ = writeln!(code, "    private MockHttpSession _session;");
    if !ctx.sql_edges.is_empty() {
        let _ = writeln!(code, "    private Mock<IDbConnection> _mockDb;");
    }
    let _ = writeln!(code);

    // Setup
    if !fw.setup_attribute().is_empty() {
        let _ = writeln!(code, "    {}", fw.setup_attribute());
    }
    let setup_name = if fw == TestFramework::XUnit {
        format!("    public {class_name}()")
    } else {
        "    public void Setup()".into()
    };
    let _ = writeln!(code, "{setup_name}");
    let _ = writeln!(code, "    {{");
    let _ = writeln!(code, "        _session = new MockHttpSession();");
    if !ctx.sql_edges.is_empty() {
        let _ = writeln!(code, "        _mockDb = new Mock<IDbConnection>();");
    }
    let _ = writeln!(code, "    }}");

    // 1. Event handler tests
    //
    // Each handler gets at least one test. Page_Load is split into
    // `IsPostBackFalse` (first render) and `IsPostBackTrue` (postback)
    // variants because those paths almost always diverge on WebForms
    // pages — state hydration on first render vs handler-driven rebind
    // on postback.
    for func in &ctx.functions {
        let fname = &func.name;
        if !is_event_handler(fname) {
            continue;
        }

        // Page_Load variants — Some(postback_flag) / None = generic.
        let variants: Vec<(&str, Option<bool>)> =
            if fname == "Page_Load" || fname.ends_with(".Page_Load") {
                vec![
                    ("IsPostBackFalse_InitializesState", Some(false)),
                    ("IsPostBackTrue_HandlesRebind", Some(true)),
                ]
            } else {
                vec![("Should_Execute_Expected_Behavior", None)]
            };

        for (variant_suffix, is_postback) in &variants {
            let test_name = format!("{fname}_{variant_suffix}");

            // Pre-compute edges relevant to this handler — shared by
            // both Arrange/Act/Assert and the coverage record below.
            let handler_reads: Vec<&Edge> = ctx
                .reads_state
                .iter()
                .filter(|e| e.source_id.contains(fname))
                .collect();
            let handler_writes: Vec<&Edge> = ctx
                .writes_state
                .iter()
                .filter(|e| e.source_id.contains(fname))
                .collect();
            let handler_sql: Vec<&Edge> = ctx
                .sql_edges
                .iter()
                .filter(|e| e.source_id.contains(fname))
                .collect();

            let _ = writeln!(code);
            // Structured summary comment — callers and reviewers can see
            // what the test verifies and why without reading the body.
            let _ = writeln!(code, "    /// <summary>");
            match is_postback {
                Some(false) => {
                    let _ = writeln!(
                        code,
                        "    /// Characterizes {fname} on first render (IsPostBack=false)."
                    );
                    let _ = writeln!(
                        code,
                        "    /// Verifies the initial state population / default branch taken when the page loads for the first time."
                    );
                }
                Some(true) => {
                    let _ = writeln!(
                        code,
                        "    /// Characterizes {fname} on postback (IsPostBack=true)."
                    );
                    let _ = writeln!(
                        code,
                        "    /// Verifies the rebind / re-read path and that session state survives the postback."
                    );
                }
                None => {
                    let _ = writeln!(
                        code,
                        "    /// Characterizes {fname}. Verifies that every session read/write and SQL"
                    );
                    let _ = writeln!(
                        code,
                        "    /// call observed during static analysis still occurs when the handler runs against mocked inputs."
                    );
                }
            }
            let _ = writeln!(code, "    /// Source: {file_path} (function {fname})");
            let _ = writeln!(code, "    /// </summary>");
            let _ = writeln!(code, "    {}", fw.test_attribute());
            let _ = writeln!(code, "    public void {test_name}()");
            let _ = writeln!(code, "    {{");

            // Arrange: mock state reads
            let _ = writeln!(code, "        // Arrange");
            for read_edge in &handler_reads {
                let key = read_edge
                    .target_id
                    .strip_prefix("state:")
                    .unwrap_or(&read_edge.target_id);
                let test_val = generate_test_value(key);
                let _ = writeln!(
                    code,
                    "        _session[\"{key}\"] = {literal}; // Read by {fname} ({desc})",
                    literal = test_val.csharp_literal,
                    desc = test_val.description
                );
            }

            // Act
            let has_sql = !handler_sql.is_empty();
            let _ = writeln!(code);
            let _ = writeln!(code, "        // Act");
            let _ = writeln!(
                code,
                "        var page = TestPageFactory.Create<{page_name}>("
            );
            let _ = writeln!(code, "            sessionState: _session,");
            if has_sql {
                let _ = writeln!(code, "            dbConnection: _mockDb?.Object);");
            } else {
                let _ = writeln!(code, "            dbConnection: null);");
            }
            if let Some(pb) = is_postback {
                let _ = writeln!(
                    code,
                    "        page.SetIsPostBack({pb}); // TestPageFactory helper — adapt if your harness uses a different hook"
                );
            }
            let _ = writeln!(code, "        page.{fname}(null, EventArgs.Empty);");

            // Assert: check state writes
            let _ = writeln!(code);
            let _ = writeln!(code, "        // Assert");
            for write_edge in &handler_writes {
                let key = write_edge
                    .target_id
                    .strip_prefix("state:")
                    .unwrap_or(&write_edge.target_id);
                let _ = writeln!(
                    code,
                    "        {}",
                    fw.assert_not_null(&format!("_session[\"{key}\"]"))
                );
            }
            for sql in &handler_sql {
                let target = &sql.target_id;
                let _ = writeln!(
                    code,
                    "        _mockDb.Verify(c => c.CreateCommand(), Times.AtLeastOnce(), \"Expected SQL call to {target}\");"
                );
            }
            if handler_writes.is_empty() && handler_sql.is_empty() {
                let _ = writeln!(
                    code,
                    "        // No state writes or SQL detected — assert page executed without throwing"
                );
                let _ = writeln!(code, "        {}", fw.assert_not_null("page"));
            }

            let _ = writeln!(code, "    }}");

            let mut covered_edges = Vec::new();
            for e in &handler_reads {
                covered_edges.push(format!("ReadsState:{} → {}", e.source_id, e.target_id));
            }
            for e in &handler_writes {
                covered_edges.push(format!("WritesState:{} → {}", e.source_id, e.target_id));
            }
            for e in &handler_sql {
                covered_edges.push(format!("SqlCalls:{} → {}", e.source_id, e.target_id));
            }

            coverage.push(TestCoverageEntry {
                test_name: test_name.clone(),
                category: TestCategory::EventHandler,
                covered_edges,
                covered_nodes: vec![func.node_id.clone()],
            });
            test_count += 1;
        }
    }

    // 1b. Auth-guard test — fires when we can identify a "permission
    // check" function in the file's function list (common the pilot corpus
    // pattern: a method named `CheckRead` / `CheckAccess` / `EnsureAuth*`
    // that SafeRedirects on failure). The test asserts an unauthorised
    // user triggers the redirect path rather than proceeding to load.
    if let Some(auth_fn) = ctx.functions.iter().find(|f| {
        let n = f.name.to_ascii_lowercase();
        n.contains("checkread")
            || n.contains("checkaccess")
            || n.contains("checkwrite")
            || n.contains("checkpermission")
            || n.contains("ensureauth")
            || n.contains("requireauth")
            || n.contains("haspermission")
    }) {
        let test_name = format!("{}_UnauthorizedUser_RedirectsViaSafeRedirect", auth_fn.name);
        let _ = writeln!(code);
        let _ = writeln!(code, "    /// <summary>");
        let _ = writeln!(
            code,
            "    /// Verifies that when the auth guard `{}` rejects the current user, the page",
            auth_fn.name
        );
        let _ = writeln!(
            code,
            "    /// short-circuits through `SafeRedirect` and does NOT proceed to load data."
        );
        let _ = writeln!(
            code,
            "    /// Source: {file_path} (auth gate `{}`). Per CLAUDE.md §11: SafeRedirect must be followed by Return.",
            auth_fn.name
        );
        let _ = writeln!(code, "    /// </summary>");
        let _ = writeln!(code, "    {}", fw.test_attribute());
        let _ = writeln!(code, "    public void {test_name}()");
        let _ = writeln!(code, "    {{");
        let _ = writeln!(
            code,
            "        // Arrange — no session user → CheckRead should fail."
        );
        let _ = writeln!(code, "        _session.Clear();");
        let _ = writeln!(code);
        let _ = writeln!(code, "        // Act");
        let _ = writeln!(
            code,
            "        var page = TestPageFactory.Create<{page_name}>(sessionState: _session, dbConnection: null);"
        );
        let _ = writeln!(
            code,
            "        // If the guard is wired into Page_Load, invoking it should have set a redirect."
        );
        let _ = writeln!(code, "        page.Page_Load(null, EventArgs.Empty);");
        let _ = writeln!(code);
        let _ = writeln!(code, "        // Assert");
        let _ = writeln!(
            code,
            "        Assert.That(page.Response?.RedirectLocation, Is.Not.Null.Or.Empty, \"Expected SafeRedirect on unauthorized access\");"
        );
        let _ = writeln!(code, "    }}");
        coverage.push(TestCoverageEntry {
            test_name,
            category: TestCategory::EventHandler,
            covered_edges: vec![],
            covered_nodes: vec![auth_fn.node_id.clone()],
        });
        test_count += 1;
    }

    // 2. Data flow tests — one per SQL query
    for (sql_test_idx, sql_edge) in ctx.sql_edges.iter().enumerate() {
        let sql_text = sql_edge
            .metadata
            .as_ref()
            .and_then(|m| m.get("sql").or_else(|| m.get("command_text")))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        let test_name = format!("DataFlow_{sql_test_idx}_Should_Return_Expected_Columns");
        let _ = writeln!(code);
        let _ = writeln!(code, "    {}", fw.test_attribute());
        let _ = writeln!(code, "    public void {test_name}()");
        let _ = writeln!(code, "    {{");
        let _ = writeln!(code, "        // Arrange");
        let _ = writeln!(code, "        // SQL: {sql_text}");

        // Parameter bindings for this query
        let params: Vec<&Edge> = ctx
            .parameter_bindings
            .iter()
            .filter(|e| e.source_id == sql_edge.source_id)
            .collect();
        for param in &params {
            let _ = writeln!(code, "        // Parameter: {}", param.target_id);
        }

        // Expected columns
        let columns: Vec<&Edge> = ctx
            .reads_column
            .iter()
            .filter(|e| e.source_id == sql_edge.source_id)
            .collect();

        let _ = writeln!(code);
        let _ = writeln!(code, "        // Act");
        let escaped_sql = sql_text.replace('"', "\\\"");
        let _ = writeln!(
            code,
            "        using var conn = TestDbFactory.CreateConnection();"
        );
        let _ = writeln!(code, "        using var cmd = conn.CreateCommand();");
        let _ = writeln!(code, "        cmd.CommandText = \"{escaped_sql}\";");
        for param in &params {
            let param_name = param
                .target_id
                .strip_prefix("param:")
                .unwrap_or(&param.target_id);
            // Phase 31: Use realistic parameter values
            let test_val = generate_test_value(param_name);
            let _ = writeln!(
                code,
                "        cmd.Parameters.AddWithValue(\"@{param_name}\", {literal}); // {desc}",
                literal = test_val.csharp_literal,
                desc = test_val.description
            );
        }
        let _ = writeln!(code, "        using var reader = cmd.ExecuteReader();");
        let _ = writeln!(
            code,
            "        var results = new List<Dictionary<string, object>>();"
        );
        let _ = writeln!(code, "        while (reader.Read()) {{");
        let _ = writeln!(
            code,
            "            var row = new Dictionary<string, object>();"
        );
        if columns.is_empty() {
            let _ = writeln!(
                code,
                "            for (int i = 0; i < reader.FieldCount; i++) row[reader.GetName(i)] = reader[i];"
            );
        } else {
            for col in &columns {
                let col_name = col.target_id.strip_prefix("col:").unwrap_or(&col.target_id);
                let _ = writeln!(
                    code,
                    "            row[\"{col_name}\"] = reader[\"{col_name}\"];"
                );
            }
        }
        let _ = writeln!(code, "            results.Add(row);");
        let _ = writeln!(code, "        }}");
        let _ = writeln!(code);
        let _ = writeln!(code, "        // Assert");
        let _ = writeln!(code, "        {}", fw.assert_not_null("results"));
        for col in &columns {
            let col_name = col.target_id.strip_prefix("col:").unwrap_or(&col.target_id);
            let _ = writeln!(
                code,
                "        if (results.Count > 0) {}",
                fw.assert_not_null(&format!("results[0][\"{col_name}\"]"))
            );
        }

        let _ = writeln!(code, "    }}");

        coverage.push(TestCoverageEntry {
            test_name,
            category: TestCategory::DataFlow,
            covered_edges: vec![format!(
                "SqlCalls:{} → {}",
                sql_edge.source_id, sql_edge.target_id
            )],
            covered_nodes: vec![],
        });
        test_count += 1;
    }

    // 3. State transition tests — per state key
    let state_keys = collect_unique_state_keys(&ctx.reads_state, &ctx.writes_state);
    for (key, readers, writers) in &state_keys {
        let safe_key = key.replace(['[', ']', '"', '\''], "");
        let test_name = format!("State_{safe_key}_Should_Be_Consistent");
        let _ = writeln!(code);
        let _ = writeln!(code, "    {}", fw.test_attribute());
        let _ = writeln!(code, "    public void {test_name}()");
        let _ = writeln!(code, "    {{");
        let _ = writeln!(code, "        // State key: {key}");
        let _ = writeln!(code, "        // Writers ({} locations):", writers.len());
        for w in writers {
            let _ = writeln!(code, "        //   - {w}");
        }
        let _ = writeln!(code, "        // Readers ({} locations):", readers.len());
        for r in readers {
            let _ = writeln!(code, "        //   - {r}");
        }
        let _ = writeln!(code);
        let _ = writeln!(code, "        // Arrange");
        let _ = writeln!(
            code,
            "        const string expectedValue = \"EXPECTED_VALUE\";"
        );
        let _ = writeln!(code);
        let _ = writeln!(code, "        // Act — trigger a writer to set the state");
        if let Some(first_writer) = writers.first() {
            let writer_fn = first_writer.rsplit("::").next().unwrap_or(first_writer);
            let _ = writeln!(
                code,
                "        var page = TestPageFactory.Create<{page_name}>("
            );
            let _ = writeln!(code, "            sessionState: _session,");
            let _ = writeln!(code, "            dbConnection: null);");
            let _ = writeln!(code, "        // Writer: {first_writer}");
            if is_event_handler(writer_fn) {
                let _ = writeln!(code, "        page.{writer_fn}(null, EventArgs.Empty);");
            } else {
                let _ = writeln!(
                    code,
                    "        // Set state directly since writer is not an event handler"
                );
                let _ = writeln!(code, "        _session[\"{safe_key}\"] = expectedValue;");
            }
        } else {
            let _ = writeln!(code, "        _session[\"{safe_key}\"] = expectedValue;");
        }
        let _ = writeln!(code);
        let _ = writeln!(
            code,
            "        // Assert — verify readers can access the value"
        );
        let _ = writeln!(
            code,
            "        {}",
            fw.assert_not_null(&format!("_session[\"{safe_key}\"]"))
        );
        let _ = writeln!(
            code,
            "        {}",
            fw.assert_eq(
                &format!("_session[\"{safe_key}\"].ToString()"),
                "expectedValue"
            )
        );
        let _ = writeln!(code, "    }}");

        coverage.push(TestCoverageEntry {
            test_name,
            category: TestCategory::StateTransition,
            covered_edges: readers
                .iter()
                .map(|r| format!("ReadsState:{r} → {key}"))
                .chain(writers.iter().map(|w| format!("WritesState:{w} → {key}")))
                .collect(),
            covered_nodes: vec![],
        });
        test_count += 1;
    }

    // 4. Navigation tests — from dependency edges (Response.Redirect / Server.Transfer)
    for dep in &ctx.dependency_edges {
        let target = &dep.target_id;
        let source = &dep.source_id;
        let test_name = format!(
            "Navigation_Should_Redirect_To_{}",
            target.replace(['/', '.', '\\'], "_")
        );
        let _ = writeln!(code);
        let _ = writeln!(code, "    {}", fw.test_attribute());
        let _ = writeln!(code, "    public void {test_name}()");
        let _ = writeln!(code, "    {{");
        let _ = writeln!(code, "        // Source: {source}");
        let _ = writeln!(code, "        // Expected navigation target: {target}");
        let _ = writeln!(code);
        let _ = writeln!(code, "        // Arrange");
        let _ = writeln!(code, "        var recorder = new MockResponseRecorder();");
        let _ = writeln!(
            code,
            "        var page = TestPageFactory.Create<{page_name}>("
        );
        let _ = writeln!(code, "            sessionState: _session,");
        let _ = writeln!(code, "            dbConnection: null,");
        let _ = writeln!(code, "            responseRecorder: recorder);");
        let _ = writeln!(code);
        let _ = writeln!(code, "        // Act — trigger navigation");
        // Try to extract a handler name from the source
        let source_fn = source.rsplit("::").next().unwrap_or(source);
        if is_event_handler(source_fn) {
            let _ = writeln!(code, "        page.{source_fn}(null, EventArgs.Empty);");
        } else {
            let _ = writeln!(
                code,
                "        // Trigger page lifecycle to invoke navigation"
            );
            let _ = writeln!(code, "        page.ProcessRequest(recorder.HttpContext);");
        }
        let _ = writeln!(code);
        let _ = writeln!(code, "        // Assert");
        let escaped_target = target.replace('"', "\\\"");
        let _ = writeln!(
            code,
            "        {}",
            fw.assert_eq("recorder.RedirectUrl", &format!("\"{escaped_target}\""))
        );
        let _ = writeln!(code, "    }}");

        coverage.push(TestCoverageEntry {
            test_name,
            category: TestCategory::Navigation,
            covered_edges: vec![format!("Dependency:{source} → {target}")],
            covered_nodes: vec![],
        });
        test_count += 1;
    }

    // 5. API contract tests — for service endpoints
    for svc_edge in &ctx.service_edges {
        let endpoint = &svc_edge.target_id;
        let safe_name = endpoint.replace(['/', '.', ':'], "_");
        let test_name = format!("Contract_{safe_name}_Returns_Expected_Schema");

        let _ = writeln!(code);
        let _ = writeln!(code, "    {}", fw.test_attribute());
        let _ = writeln!(code, "    public async Task {test_name}()");
        let _ = writeln!(code, "    {{");
        let _ = writeln!(code, "        // Service endpoint: {endpoint}");
        let _ = writeln!(code, "        var client = new HttpClient();");
        let _ = writeln!(code);
        let _ = writeln!(code, "        // Act");
        let _ = writeln!(
            code,
            "        var response = await client.GetAsync(\"{endpoint}\");"
        );
        let _ = writeln!(code);
        let _ = writeln!(code, "        // Assert");
        let _ = writeln!(
            code,
            "        {}",
            fw.assert_eq("(int)response.StatusCode", "200")
        );
        let _ = writeln!(
            code,
            "        {}",
            fw.assert_not_null("await response.Content.ReadAsStringAsync()")
        );
        let _ = writeln!(code, "    }}");

        coverage.push(TestCoverageEntry {
            test_name,
            category: TestCategory::ApiContract,
            covered_edges: vec![format!("Service:{} → {}", svc_edge.source_id, endpoint)],
            covered_nodes: vec![],
        });
        test_count += 1;
    }

    // Phase 31: Multi-scenario tests — missing state + boundary values
    // Generate for each event handler that has state reads
    for func in &ctx.functions {
        let fname = &func.name;
        if !is_event_handler(fname) {
            continue;
        }

        let handler_reads: Vec<&Edge> = ctx
            .reads_state
            .iter()
            .filter(|e| e.source_id.contains(fname))
            .collect();

        if handler_reads.is_empty() {
            continue;
        }

        // Missing state test
        let missing_test = format!("{fname}_Should_Handle_Missing_State");
        let _ = writeln!(code);
        let _ = writeln!(code, "    {}", fw.test_attribute());
        let _ = writeln!(code, "    public void {missing_test}()");
        let _ = writeln!(code, "    {{");
        let _ = writeln!(
            code,
            "        // Arrange — state keys intentionally NOT set"
        );
        let _ = writeln!(
            code,
            "        // This characterizes behavior when session state is missing"
        );
        let _ = writeln!(code);
        let _ = writeln!(code, "        // Act");
        let _ = writeln!(
            code,
            "        var page = TestPageFactory.Create<{page_name}>("
        );
        let _ = writeln!(code, "            sessionState: _session,");
        let _ = writeln!(code, "            dbConnection: null);");
        let _ = writeln!(
            code,
            "        // NOTE: If legacy code crashes with NullReferenceException,"
        );
        let _ = writeln!(
            code,
            "        // that IS the expected behavior to characterize."
        );
        let _ = writeln!(code, "        try {{");
        let _ = writeln!(code, "            page.{fname}(null, EventArgs.Empty);");
        let _ = writeln!(code, "        }} catch (NullReferenceException) {{");
        let _ = writeln!(
            code,
            "            // Document: legacy code does NOT handle missing state gracefully"
        );
        let _ = writeln!(code, "            return;");
        let _ = writeln!(code, "        }}");
        let _ = writeln!(
            code,
            "        // If we reach here: legacy code handles missing state"
        );
        let _ = writeln!(code, "        {}", fw.assert_not_null("page"));
        let _ = writeln!(code, "    }}");

        coverage.push(TestCoverageEntry {
            test_name: missing_test,
            category: TestCategory::StateTransition,
            covered_edges: handler_reads
                .iter()
                .map(|e| format!("MissingState:{} → {}", e.source_id, e.target_id))
                .collect(),
            covered_nodes: vec![func.node_id.clone()],
        });
        test_count += 1;

        // Boundary values test
        let boundary_test = format!("{fname}_Should_Handle_Boundary_Values");
        let _ = writeln!(code);
        let _ = writeln!(code, "    {}", fw.test_attribute());
        let _ = writeln!(code, "    public void {boundary_test}()");
        let _ = writeln!(code, "    {{");
        let _ = writeln!(code, "        // Arrange — boundary/edge-case values");
        for read_edge in &handler_reads {
            let key = read_edge
                .target_id
                .strip_prefix("state:")
                .unwrap_or(&read_edge.target_id);
            let test_val = generate_test_value(key);
            let _ = writeln!(
                code,
                "        _session[\"{key}\"] = {boundary}; // boundary value for {key}",
                boundary = test_val.boundary_variant
            );
        }
        let _ = writeln!(code);
        let _ = writeln!(code, "        // Act");
        let _ = writeln!(
            code,
            "        var page = TestPageFactory.Create<{page_name}>("
        );
        let _ = writeln!(code, "            sessionState: _session,");
        let _ = writeln!(code, "            dbConnection: null);");
        let _ = writeln!(code, "        page.{fname}(null, EventArgs.Empty);");
        let _ = writeln!(code);
        let _ = writeln!(code, "        // Assert — characterize boundary behavior");
        let _ = writeln!(code, "        {}", fw.assert_not_null("page"));
        let _ = writeln!(code, "    }}");

        coverage.push(TestCoverageEntry {
            test_name: boundary_test,
            category: TestCategory::StateTransition,
            covered_edges: handler_reads
                .iter()
                .map(|e| format!("Boundary:{} → {}", e.source_id, e.target_id))
                .collect(),
            covered_nodes: vec![func.node_id.clone()],
        });
        test_count += 1;
    }

    let _ = writeln!(code, "}}");

    // Phase 31: Generate test fixture class
    let state_keys_for_fixtures = collect_unique_state_keys(&ctx.reads_state, &ctx.writes_state);
    if !state_keys_for_fixtures.is_empty() {
        let _ = writeln!(code);
        let fixtures = generate_test_fixtures(&page_name, &state_keys_for_fixtures);
        code.push_str(&fixtures);
    }

    if test_count == 0 {
        warnings.push(
            "No extraction data found for test generation — ensure the file has been indexed"
                .into(),
        );
    }

    Ok(CharacterizationTestResult {
        test_code: code,
        coverage_map: coverage,
        framework: framework_str.to_string(),
        test_count,
        warnings,
    })
}

// ─── Phase 31: Test data generator ────────────────────────────────────────────

/// Generate a realistic test value for a state key or column name.
pub fn generate_test_value(key_name: &str) -> TestValue {
    let lower = key_name.to_lowercase();

    // Security-sensitive keys → redacted
    if lower.contains("password")
        || lower.contains("secret")
        || lower.contains("token")
        || lower.contains("apikey")
        || lower.contains("api_key")
    {
        return TestValue {
            csharp_literal: "\"REDACTED_TEST_TOKEN\"".into(),
            csharp_type: "string".into(),
            description: "Never use real credentials in tests".into(),
            null_variant: "null".into(),
            boundary_variant: "\"\"".into(),
        };
    }

    // GUID/UUID
    if lower.ends_with("guid") || lower.ends_with("uuid") {
        return TestValue {
            csharp_literal: "Guid.Parse(\"550e8400-e29b-41d4-a716-446655440000\")".into(),
            csharp_type: "Guid".into(),
            description: "GUID identifier".into(),
            null_variant: "Guid.Empty".into(),
            boundary_variant: "Guid.Empty".into(),
        };
    }

    // Integer IDs
    if lower.ends_with("id") || lower == "id" {
        let context_prefix = extract_context_prefix(&lower);
        return TestValue {
            csharp_literal: "42".into(),
            csharp_type: "int".into(),
            description: format!("{context_prefix} identifier"),
            null_variant: "0".into(),
            boundary_variant: "-1".into(),
        };
    }

    // Email
    if lower.contains("email") {
        return TestValue {
            csharp_literal: "\"test@example.com\"".into(),
            csharp_type: "string".into(),
            description: "Email address".into(),
            null_variant: "null".into(),
            boundary_variant: "\"\"".into(),
        };
    }

    // Name (contextual)
    if lower.ends_with("name") || lower.ends_with("title") {
        let context = extract_context_prefix(&lower);
        let value = format!("Test {context}");
        return TestValue {
            csharp_literal: format!("\"{value}\""),
            csharp_type: "string".into(),
            description: format!("{context} name"),
            null_variant: "null".into(),
            boundary_variant: "\"\"".into(),
        };
    }

    // Date/time
    if lower.ends_with("date")
        || lower.ends_with("time")
        || lower.ends_with("at")
        || lower.ends_with("on")
        || lower.starts_with("created")
        || lower.starts_with("modified")
        || lower.starts_with("updated")
    {
        return TestValue {
            csharp_literal: "new DateTime(2024, 1, 15)".into(),
            csharp_type: "DateTime".into(),
            description: "Date/time value".into(),
            null_variant: "DateTime.MinValue".into(),
            boundary_variant: "DateTime.MinValue".into(),
        };
    }

    // Money/amounts
    if lower.ends_with("amount")
        || lower.ends_with("total")
        || lower.ends_with("price")
        || lower.ends_with("cost")
        || lower.ends_with("balance")
    {
        return TestValue {
            csharp_literal: "99.99m".into(),
            csharp_type: "decimal".into(),
            description: "Monetary value".into(),
            null_variant: "0m".into(),
            boundary_variant: "-0.01m".into(),
        };
    }

    // Counts
    if lower.ends_with("count") || lower.ends_with("qty") || lower.ends_with("quantity") {
        return TestValue {
            csharp_literal: "5".into(),
            csharp_type: "int".into(),
            description: "Count/quantity".into(),
            null_variant: "0".into(),
            boundary_variant: "-1".into(),
        };
    }

    // Boolean
    if lower.starts_with("is")
        || lower.starts_with("has")
        || lower.ends_with("active")
        || lower.ends_with("enabled")
    {
        return TestValue {
            csharp_literal: "true".into(),
            csharp_type: "bool".into(),
            description: "Boolean flag".into(),
            null_variant: "false".into(),
            boundary_variant: "false".into(),
        };
    }

    // Status (string enum pattern)
    if lower.contains("status") {
        return TestValue {
            csharp_literal: "\"Active\"".into(),
            csharp_type: "string".into(),
            description: "Status value".into(),
            null_variant: "null".into(),
            boundary_variant: "\"\"".into(),
        };
    }

    // URL/Path
    if lower.ends_with("url") || lower.ends_with("uri") || lower.ends_with("link") {
        return TestValue {
            csharp_literal: "\"https://example.com\"".into(),
            csharp_type: "string".into(),
            description: "URL".into(),
            null_variant: "null".into(),
            boundary_variant: "\"\"".into(),
        };
    }
    if lower.ends_with("path") {
        return TestValue {
            csharp_literal: "\"/test/path\"".into(),
            csharp_type: "string".into(),
            description: "File/URL path".into(),
            null_variant: "null".into(),
            boundary_variant: "\"\"".into(),
        };
    }

    // Phone
    if lower.contains("phone") {
        return TestValue {
            csharp_literal: "\"555-0100\"".into(),
            csharp_type: "string".into(),
            description: "Phone number".into(),
            null_variant: "null".into(),
            boundary_variant: "\"\"".into(),
        };
    }

    // Zip/Postal
    if lower.contains("zip") || lower.contains("postal") {
        return TestValue {
            csharp_literal: "\"12345\"".into(),
            csharp_type: "string".into(),
            description: "Postal code".into(),
            null_variant: "null".into(),
            boundary_variant: "\"\"".into(),
        };
    }

    // Description/Notes
    if lower.contains("description") || lower.contains("notes") || lower.contains("comment") {
        return TestValue {
            csharp_literal: "\"Test description for characterization\"".into(),
            csharp_type: "string".into(),
            description: "Text content".into(),
            null_variant: "null".into(),
            boundary_variant: "\"\"".into(),
        };
    }

    // Default: string fallback
    TestValue {
        csharp_literal: format!("\"test_{key_name}\""),
        csharp_type: "string".into(),
        description: format!("String value — verify type for {key_name}"),
        null_variant: "null".into(),
        boundary_variant: "\"\"".into(),
    }
}

/// A test value with type information and boundary variants.
#[derive(Debug, Clone, Serialize)]
pub struct TestValue {
    /// C# literal for the test value.
    pub csharp_literal: String,
    /// C# type name.
    pub csharp_type: String,
    /// Description for the developer.
    pub description: String,
    /// Null/empty variant for negative tests.
    pub null_variant: String,
    /// Boundary/edge-case variant.
    pub boundary_variant: String,
}

fn extract_context_prefix(lower: &str) -> String {
    // "username" → "User", "productname" → "Product", "orderid" → "Order"
    let suffixes = [
        "name", "title", "id", "email", "date", "time", "count", "amount", "total", "price",
        "status", "url", "path", "phone", "code", "type", "guid", "uuid", "at", "on",
    ];
    for suffix in &suffixes {
        if lower.ends_with(suffix) && lower.len() > suffix.len() {
            let prefix = &lower[..lower.len() - suffix.len()];
            if !prefix.is_empty() {
                let mut chars = prefix.chars();
                let first = chars.next().unwrap_or('t').to_uppercase().to_string();
                return format!("{first}{}", chars.as_str());
            }
        }
    }
    "Item".into()
}

/// Generate a test fixture class with static test data.
pub fn generate_test_fixtures(
    page_name: &str,
    state_keys: &[(String, Vec<String>, Vec<String>)],
) -> String {
    let mut code = String::with_capacity(1024);
    let _ = writeln!(code, "public static class {page_name}TestFixtures");
    let _ = writeln!(code, "{{");

    for (key, _, _) in state_keys {
        let safe_key = key.replace(['[', ']', '"', '\'', ':'], "");
        let field_name = format!("Valid{}", to_pascal(&safe_key));
        let test_val = generate_test_value(&safe_key);
        let csharp_type = &test_val.csharp_type;
        let literal = &test_val.csharp_literal;
        let _ = writeln!(
            code,
            "    public static readonly {csharp_type} {field_name} = {literal};"
        );
    }

    let _ = writeln!(code);
    let _ = writeln!(
        code,
        "    public static MockHttpSession CreateAuthenticatedSession()"
    );
    let _ = writeln!(code, "    {{");
    let _ = writeln!(code, "        var session = new MockHttpSession();");
    for (key, _, _) in state_keys {
        let safe_key = key.replace(['[', ']', '"', '\'', ':'], "");
        let field_name = format!("Valid{}", to_pascal(&safe_key));
        let _ = writeln!(code, "        session[\"{safe_key}\"] = {field_name};");
    }
    let _ = writeln!(code, "        return session;");
    let _ = writeln!(code, "    }}");
    let _ = writeln!(code, "}}");
    code
}

fn to_pascal(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut cap_next = true;
    for c in s.chars() {
        if c == '_' || c == ' ' || c == '-' {
            cap_next = true;
            continue;
        }
        if cap_next {
            result.extend(c.to_uppercase());
            cap_next = false;
        } else {
            result.push(c);
        }
    }
    result
}

// ─── Internals ────────────────────────────────────────────────────────────────

fn collect_test_context(
    graph: &Arc<GraphStore>,
    project_id: &str,
    file_path: &str,
) -> anyhow::Result<TestGenContext> {
    // Push the `file_path` filter into the graph query so we don't
    // over-fetch — `query_nodes` does case-insensitive slash-normalised
    // substring matching (see `contains_case_insensitive_path`), which
    // tolerates Windows-style `\` in the caller's input and avoids
    // capping out at the 5000-node limit on large projects (pilot corpus has
    // ~24k function nodes, so the old `query_nodes(None, None, None,
    // 5000)` silently discarded ~78% of them).
    let all_fns = graph.query_nodes(project_id, Some("function"), None, Some(file_path), 10_000)?;

    // Second pass: tighten the match to an exact-path comparison with
    // slash + case normalisation so two unrelated files that happen to
    // share a common suffix don't bleed together. A file stored as
    // `Site/App_Code/foo.vb` must match both `Site/App_Code/foo.vb` and
    // `Site\App_Code\foo.vb` supplied by the caller, but must NOT match
    // `Other/App_Code/foo.vb`.
    let input_fp = file_path.replace('\\', "/").to_ascii_lowercase();
    let all_fns_count = all_fns.len();
    let functions: Vec<Node> = all_fns
        .iter()
        .filter(|n| {
            let node_fp = n.file_path.as_str().replace('\\', "/").to_ascii_lowercase();
            node_fp == input_fp
                || node_fp.ends_with(&format!("/{input_fp}"))
                || input_fp.ends_with(&format!("/{node_fp}"))
        })
        .cloned()
        .collect();

    tracing::info!(
        project_id = %project_id,
        file_path = %file_path,
        all_fns = all_fns_count,
        matched_fns = functions.len(),
        "generate_characterization_tests: collected function nodes for file"
    );
    if functions.is_empty() && all_fns_count > 0 {
        let samples: Vec<&str> = all_fns
            .iter()
            .take(5)
            .map(|n| n.file_path.as_str())
            .collect();
        tracing::warn!(
            project_id = %project_id,
            file_path = %file_path,
            sample_node_paths = ?samples,
            "generate_characterization_tests: 0 functions matched the target file_path — \
             check path-casing / prefix mismatch between the caller and the indexed paths"
        );
    }

    let filter = |edges: Vec<Edge>| -> Vec<Edge> {
        edges
            .into_iter()
            .filter(|e| {
                e.source_id.contains(file_path)
                    || e.metadata
                        .as_ref()
                        .and_then(|m| m.get("file_path"))
                        .and_then(|v| v.as_str())
                        .is_some_and(|fp| fp == file_path)
            })
            .collect()
    };

    let sql_edges = filter(graph.list_edges_by_kind(project_id, EdgeKind::SqlCalls, 10_000)?);
    let reads_state = filter(graph.list_edges_by_kind(project_id, EdgeKind::ReadsState, 10_000)?);
    let writes_state =
        filter(graph.list_edges_by_kind(project_id, EdgeKind::WritesState, 10_000)?);
    let reads_column =
        filter(graph.list_edges_by_kind(project_id, EdgeKind::ReadsColumn, 10_000)?);
    let queries_table =
        filter(graph.list_edges_by_kind(project_id, EdgeKind::QueriesTable, 10_000)?);
    let parameter_bindings =
        filter(graph.list_edges_by_kind(project_id, EdgeKind::ParameterBinding, 10_000)?);
    let triggers_postback =
        filter(graph.list_edges_by_kind(project_id, EdgeKind::TriggersPostback, 10_000)?);
    let dependency_edges =
        filter(graph.list_edges_by_kind(project_id, EdgeKind::Dependency, 10_000)?);

    let mut service_edges = Vec::new();
    for kind in [
        EdgeKind::ExposesWebService,
        EdgeKind::ExposesHttpHandler,
        EdgeKind::ExposesWcfService,
    ] {
        service_edges.extend(filter(graph.list_edges_by_kind(project_id, kind, 1000)?));
    }

    let all_conns = graph.query_nodes(project_id, Some("connection_string"), None, None, 500)?;
    let connection_strings: Vec<Node> = all_conns
        .into_iter()
        .filter(|n| n.file_path.as_str() == file_path)
        .collect();

    Ok(TestGenContext {
        functions,
        sql_edges,
        reads_state,
        writes_state,
        reads_column,
        queries_table,
        parameter_bindings,
        triggers_postback,
        dependency_edges,
        service_edges,
        connection_strings,
    })
}

fn emit_helper_classes(code: &mut String, _page_name: &str) {
    // TestPageFactory
    let _ = writeln!(code, "public static class TestPageFactory");
    let _ = writeln!(code, "{{");
    let _ = writeln!(code, "    public static T Create<T>(");
    let _ = writeln!(code, "        MockHttpSession sessionState = null,");
    let _ = writeln!(code, "        IDbConnection dbConnection = null,");
    let _ = writeln!(
        code,
        "        MockResponseRecorder responseRecorder = null) where T : System.Web.UI.Page, new()"
    );
    let _ = writeln!(code, "    {{");
    let _ = writeln!(code, "        var page = new T();");
    let _ = writeln!(
        code,
        "        var context = new MockHttpContext(sessionState ?? new MockHttpSession(), responseRecorder);"
    );
    let _ = writeln!(
        code,
        "        // Inject HttpContext via reflection for testability"
    );
    let _ = writeln!(
        code,
        "        var ctxField = typeof(System.Web.UI.Page).GetField(\"_context\","
    );
    let _ = writeln!(
        code,
        "            System.Reflection.BindingFlags.NonPublic | System.Reflection.BindingFlags.Instance);"
    );
    let _ = writeln!(code, "        ctxField?.SetValue(page, context);");
    let _ = writeln!(code, "        return page;");
    let _ = writeln!(code, "    }}");
    let _ = writeln!(code, "}}");
    let _ = writeln!(code);

    // MockHttpSession
    let _ = writeln!(code, "public class MockHttpSession : IHttpSessionState");
    let _ = writeln!(code, "{{");
    let _ = writeln!(
        code,
        "    private readonly Dictionary<string, object> _store = new Dictionary<string, object>();"
    );
    let _ = writeln!(code, "    public object this[string key]");
    let _ = writeln!(code, "    {{");
    let _ = writeln!(
        code,
        "        get => _store.ContainsKey(key) ? _store[key] : null;"
    );
    let _ = writeln!(code, "        set => _store[key] = value;");
    let _ = writeln!(code, "    }}");
    let _ = writeln!(code, "    public int Count => _store.Count;");
    let _ = writeln!(
        code,
        "    // Minimal IHttpSessionState implementation — extend as needed"
    );
    let _ = writeln!(code, "}}");
    let _ = writeln!(code);

    // TestDbFactory
    let _ = writeln!(code, "public static class TestDbFactory");
    let _ = writeln!(code, "{{");
    let _ = writeln!(
        code,
        "    // Set this to your legacy database connection string for characterization tests"
    );
    let _ = writeln!(
        code,
        "    public static string ConnectionString {{ get; set; }} ="
    );
    let _ = writeln!(
        code,
        "        System.Configuration.ConfigurationManager.ConnectionStrings[\"DefaultConnection\"]?.ConnectionString"
    );
    let _ = writeln!(
        code,
        "        ?? \"Server=localhost;Database=LegacyDb;Trusted_Connection=True;\";"
    );
    let _ = writeln!(code);
    let _ = writeln!(code, "    public static IDbConnection CreateConnection()");
    let _ = writeln!(code, "    {{");
    let _ = writeln!(
        code,
        "        var conn = new System.Data.SqlClient.SqlConnection(ConnectionString);"
    );
    let _ = writeln!(code, "        conn.Open();");
    let _ = writeln!(code, "        return conn;");
    let _ = writeln!(code, "    }}");
    let _ = writeln!(code, "}}");
    let _ = writeln!(code);

    // MockResponseRecorder
    let _ = writeln!(code, "public class MockResponseRecorder");
    let _ = writeln!(code, "{{");
    let _ = writeln!(
        code,
        "    public string RedirectUrl {{ get; private set; }}"
    );
    let _ = writeln!(code, "    public int StatusCode {{ get; set; }} = 200;");
    let _ = writeln!(code, "    public HttpContext HttpContext {{ get; }}");
    let _ = writeln!(code);
    let _ = writeln!(code, "    public MockResponseRecorder()");
    let _ = writeln!(code, "    {{");
    let _ = writeln!(
        code,
        "        // Build a minimal HttpContext that captures redirects"
    );
    let _ = writeln!(code, "        HttpContext = new HttpContext(");
    let _ = writeln!(
        code,
        "            new HttpRequest(\"test\", \"http://localhost/test\", \"\"),"
    );
    let _ = writeln!(
        code,
        "            new HttpResponse(new System.IO.StringWriter()));"
    );
    let _ = writeln!(code, "    }}");
    let _ = writeln!(code);
    let _ = writeln!(code, "    public void Redirect(string url)");
    let _ = writeln!(code, "    {{");
    let _ = writeln!(code, "        RedirectUrl = url;");
    let _ = writeln!(code, "    }}");
    let _ = writeln!(code, "}}");
    let _ = writeln!(code);

    // MockHttpContext
    let _ = writeln!(code, "public class MockHttpContext : HttpContextBase");
    let _ = writeln!(code, "{{");
    let _ = writeln!(code, "    private readonly MockHttpSession _session;");
    let _ = writeln!(code, "    private readonly MockResponseRecorder _recorder;");
    let _ = writeln!(code);
    let _ = writeln!(
        code,
        "    public MockHttpContext(MockHttpSession session, MockResponseRecorder recorder = null)"
    );
    let _ = writeln!(code, "    {{");
    let _ = writeln!(code, "        _session = session;");
    let _ = writeln!(code, "        _recorder = recorder;");
    let _ = writeln!(code, "    }}");
    let _ = writeln!(code);
    let _ = writeln!(
        code,
        "    public override HttpSessionStateBase Session => new HttpSessionStateWrapper(_session);"
    );
    let _ = writeln!(code, "}}");
}

fn is_event_handler(name: &str) -> bool {
    // Covers the full set of WebForms event-attribute suffixes that the
    // markup extractor recognises, plus the lifecycle hooks and a few
    // common codebehind-only event names. Keep this list in sync with
    // the markup-side `EVENT_ATTR_RE` in `engram_index::webforms` so
    // every wired event on an ASPX page produces a characterization test.
    const SUFFIXES: &[&str] = &[
        // Core input / action events
        "_Click",
        "_ServerClick",
        "_Command",
        "_Changed",
        "_TextChanged",
        "_CheckedChanged",
        "_ValueChanged",
        "_SelectedIndexChanged",
        "_ServerChange",
        // Validators
        "_ServerValidate",
        // Page / control lifecycle
        "_Load",
        "_Init",
        "_PreRender",
        "_PreInit",
        "_Unload",
        // Data-binding callbacks
        "_DataBound",
        "_ItemDataBound",
        "_RowDataBound",
        "_Selecting",
        "_Inserting",
        "_Updating",
        "_Deleting",
        "_Selected",
        "_Inserted",
        "_Updated",
        "_Deleted",
        // Grid / list commands + row editing
        "_RowCommand",
        "_RowEditing",
        "_RowUpdating",
        "_RowUpdated",
        "_RowDeleting",
        "_RowDeleted",
        "_RowCancelingEdit",
        "_ItemCommand",
        // Grid / list paging + sorting
        "_PageIndexChanging",
        "_PageIndexChanged",
        "_Sorting",
        "_Sorted",
    ];
    SUFFIXES.iter().any(|sfx| name.contains(sfx))
}

fn extract_page_name(file_path: &str) -> String {
    file_path
        .replace('\\', "/")
        .rsplit('/')
        .next()
        .unwrap_or(file_path)
        .replace(".aspx.cs", "")
        .replace(".aspx.vb", "")
        .replace(".aspx", "")
        .replace(".ascx.cs", "")
        .replace(".ascx.vb", "")
        .replace(".ascx", "")
        .replace(".cs", "")
        .replace(".vb", "")
}

fn collect_unique_state_keys(
    reads: &[Edge],
    writes: &[Edge],
) -> Vec<(String, Vec<String>, Vec<String>)> {
    let mut key_readers: BTreeMap<String, HashSet<String>> = BTreeMap::new();
    let mut key_writers: BTreeMap<String, HashSet<String>> = BTreeMap::new();

    for e in reads {
        let key = e
            .target_id
            .strip_prefix("state:")
            .unwrap_or(&e.target_id)
            .to_string();
        key_readers
            .entry(key)
            .or_default()
            .insert(e.source_id.clone());
    }
    for e in writes {
        let key = e
            .target_id
            .strip_prefix("state:")
            .unwrap_or(&e.target_id)
            .to_string();
        key_writers
            .entry(key)
            .or_default()
            .insert(e.source_id.clone());
    }

    let mut all_keys: HashSet<String> = HashSet::new();
    all_keys.extend(key_readers.keys().cloned());
    all_keys.extend(key_writers.keys().cloned());

    let mut result: Vec<(String, Vec<String>, Vec<String>)> = all_keys
        .into_iter()
        .map(|k| {
            let r = key_readers
                .get(&k)
                .map(|s| s.iter().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            let w = key_writers
                .get(&k)
                .map(|s| s.iter().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            (k, r, w)
        })
        .collect();
    result.sort_by(|a, b| a.0.cmp(&b.0));
    result
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_framework_attributes() {
        let nunit = TestFramework::NUnit;
        assert_eq!(nunit.test_attribute(), "[Test]");
        assert_eq!(nunit.fixture_attribute(), "[TestFixture]");

        let xunit = TestFramework::XUnit;
        assert_eq!(xunit.test_attribute(), "[Fact]");
        assert!(xunit.fixture_attribute().is_empty());

        let mstest = TestFramework::MSTest;
        assert_eq!(mstest.test_attribute(), "[TestMethod]");
        assert_eq!(mstest.fixture_attribute(), "[TestClass]");
    }

    #[test]
    fn test_framework_from_str() {
        assert_eq!(
            TestFramework::from_str("nunit").unwrap(),
            TestFramework::NUnit
        );
        assert_eq!(
            TestFramework::from_str("xunit").unwrap(),
            TestFramework::XUnit
        );
        assert_eq!(
            TestFramework::from_str("mstest").unwrap(),
            TestFramework::MSTest
        );
        assert!(TestFramework::from_str("unknown").is_err()); // fail-closed
    }

    #[test]
    fn test_framework_assert_eq() {
        let nunit = TestFramework::NUnit;
        assert!(
            nunit
                .assert_eq("a", "b")
                .contains("Assert.That(a, Is.EqualTo(b))")
        );

        let xunit = TestFramework::XUnit;
        assert!(xunit.assert_eq("a", "b").contains("Assert.Equal(b, a)"));

        let mstest = TestFramework::MSTest;
        assert!(mstest.assert_eq("a", "b").contains("Assert.AreEqual(b, a)"));
    }

    #[test]
    fn is_event_handler_detection() {
        assert!(is_event_handler("Button_Click"));
        assert!(is_event_handler("GridView_RowCommand"));
        assert!(is_event_handler("DropDown_SelectedIndexChanged"));
        assert!(is_event_handler("Page_Load"));
        assert!(!is_event_handler("LoadData"));
        assert!(!is_event_handler("ProcessOrder"));
    }

    /// The expanded suffix list must cover the pilot-realistic event
    /// shapes that the markup extractor's `EVENT_ATTR_RE` emits. Before
    /// this expansion the characterization service generated 3 tests
    /// for a page with 8 wired handlers because half its events
    /// (`_PageIndexChanged`, `_Selecting`, `_Sorting`, `_RowDataBound`,
    /// etc.) weren't in the allow-list and silently dropped.
    #[test]
    fn is_event_handler_recognises_grid_and_linq_events() {
        // Grid paging / sorting / row editing.
        assert!(is_event_handler("gvMain_PageIndexChanging"));
        assert!(is_event_handler("gvMain_PageIndexChanged"));
        assert!(is_event_handler("gvMain_Sorting"));
        assert!(is_event_handler("gvMain_RowEditing"));
        assert!(is_event_handler("gvMain_RowUpdating"));
        assert!(is_event_handler("gvMain_RowDeleting"));
        assert!(is_event_handler("gvMain_RowCancelingEdit"));
        // LinqDataSource / ObjectDataSource lifecycle.
        assert!(is_event_handler("linqSource_Selecting"));
        assert!(is_event_handler("linqSource_Inserting"));
        assert!(is_event_handler("linqSource_Updating"));
        assert!(is_event_handler("linqSource_Deleting"));
        assert!(is_event_handler("linqSource_Selected"));
        // Validators + misc.
        assert!(is_event_handler("cvEmail_ServerValidate"));
        assert!(is_event_handler("txtSearch_TextChanged"));
        assert!(is_event_handler("chkAgree_CheckedChanged"));
        assert!(is_event_handler("btnSave_ServerClick"));
        // Non-handlers still rejected.
        assert!(!is_event_handler("LoadData"));
        assert!(!is_event_handler("ComputeTotal"));
    }

    /// An auth-guard name-detector sanity check. The end-to-end
    /// generator is exercised by the existing integration suite with a
    /// real `GraphStore`; here we only assert the name-classification
    /// that drives whether an auth test gets emitted.
    #[test]
    fn auth_guard_name_detector_recognises_common_patterns() {
        fn looks_like_auth_guard(name: &str) -> bool {
            let n = name.to_ascii_lowercase();
            n.contains("checkread")
                || n.contains("checkaccess")
                || n.contains("checkwrite")
                || n.contains("checkpermission")
                || n.contains("ensureauth")
                || n.contains("requireauth")
                || n.contains("haspermission")
        }
        assert!(looks_like_auth_guard("CheckRead"));
        assert!(looks_like_auth_guard("Admin_CheckAccess"));
        assert!(looks_like_auth_guard("EnsureAuthenticated"));
        assert!(looks_like_auth_guard("RequireAuth"));
        assert!(looks_like_auth_guard("HasPermission"));
        // Non-auth helpers are not flagged.
        assert!(!looks_like_auth_guard("LoadUserPreferences"));
        assert!(!looks_like_auth_guard("SaveOrder"));
    }

    #[test]
    fn extract_page_name_variants() {
        assert_eq!(extract_page_name("Orders.aspx.cs"), "Orders");
        assert_eq!(extract_page_name("Admin/Users.aspx.vb"), "Users");
        assert_eq!(extract_page_name("Controls/Header.ascx"), "Header");
    }

    #[test]
    fn collect_state_keys_groups() {
        let reads = vec![
            Edge {
                source_id: "fn:A".into(),
                target_id: "state:UserId".into(),
                namespace: "t".into(),
                language: "cs".into(),
                edge_kind: EdgeKind::ReadsState,
                weight: 1,
                generation: 1,
                metadata: None,
                updated_at_ms: 0,
            },
            Edge {
                source_id: "fn:B".into(),
                target_id: "state:UserId".into(),
                namespace: "t".into(),
                language: "cs".into(),
                edge_kind: EdgeKind::ReadsState,
                weight: 1,
                generation: 1,
                metadata: None,
                updated_at_ms: 0,
            },
        ];
        let writes = vec![Edge {
            source_id: "fn:C".into(),
            target_id: "state:UserId".into(),
            namespace: "t".into(),
            language: "cs".into(),
            edge_kind: EdgeKind::WritesState,
            weight: 1,
            generation: 1,
            metadata: None,
            updated_at_ms: 0,
        }];

        let keys = collect_unique_state_keys(&reads, &writes);
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].0, "UserId");
        assert_eq!(keys[0].1.len(), 2); // 2 readers
        assert_eq!(keys[0].2.len(), 1); // 1 writer
    }

    #[test]
    fn test_category_serialization() {
        let json = serde_json::to_string(&TestCategory::EventHandler).unwrap_or_default();
        assert!(json.contains("event_handler"));
    }

    // Phase 31: Test data realism tests

    #[test]
    fn test_data_id_key_generates_integer() {
        let val = generate_test_value("UserId");
        assert_eq!(val.csharp_type, "int");
        assert_eq!(val.csharp_literal, "42");
    }

    #[test]
    fn test_data_name_key_generates_contextual_string() {
        let val = generate_test_value("UserName");
        assert_eq!(val.csharp_type, "string");
        assert!(val.csharp_literal.contains("Test User"));
    }

    #[test]
    fn test_data_email_key_generates_email() {
        let val = generate_test_value("UserEmail");
        assert_eq!(val.csharp_type, "string");
        assert!(val.csharp_literal.contains("test@example.com"));
    }

    #[test]
    fn test_data_date_key_generates_datetime() {
        let val = generate_test_value("OrderDate");
        assert_eq!(val.csharp_type, "DateTime");
        assert!(val.csharp_literal.contains("DateTime"));
    }

    #[test]
    fn test_data_amount_key_generates_decimal() {
        let val = generate_test_value("TotalAmount");
        assert_eq!(val.csharp_type, "decimal");
        assert!(val.csharp_literal.contains("99.99m"));
    }

    #[test]
    fn test_data_unknown_key_fallback() {
        let val = generate_test_value("XyzFoo");
        assert_eq!(val.csharp_type, "string");
        assert!(val.description.contains("verify type"));
    }

    #[test]
    fn test_data_negative_variant_for_string() {
        let val = generate_test_value("UserName");
        assert_eq!(val.null_variant, "null");
        assert_eq!(val.boundary_variant, "\"\"");
    }

    #[test]
    fn test_data_negative_variant_for_int() {
        let val = generate_test_value("OrderId");
        assert_eq!(val.null_variant, "0");
        assert_eq!(val.boundary_variant, "-1");
    }

    #[test]
    fn test_data_no_passwords_in_values() {
        let val = generate_test_value("UserPassword");
        assert!(val.csharp_literal.contains("REDACTED"));
        assert!(val.description.contains("Never use real credentials"));
    }

    #[test]
    fn test_data_status_generates_enum_pattern() {
        let val = generate_test_value("OrderStatus");
        assert_eq!(val.csharp_literal, "\"Active\"");
    }

    #[test]
    fn test_data_phone_generates_phone() {
        let val = generate_test_value("WorkPhone");
        assert!(val.csharp_literal.contains("555-0100"));
    }

    #[test]
    fn test_data_guid_key() {
        let val = generate_test_value("OrderGuid");
        assert_eq!(val.csharp_type, "Guid");
        assert!(val.csharp_literal.contains("Guid.Parse"));
    }

    #[test]
    fn test_fixture_class_generated() {
        let state_keys = vec![
            ("UserId".into(), vec!["fn:A".into()], vec!["fn:B".into()]),
            ("UserName".into(), vec!["fn:A".into()], vec![]),
        ];
        let fixtures = generate_test_fixtures("OrderPage", &state_keys);
        assert!(fixtures.contains("OrderPageTestFixtures"));
        assert!(fixtures.contains("ValidUserId"));
        assert!(fixtures.contains("ValidUserName"));
        assert!(fixtures.contains("CreateAuthenticatedSession"));
    }

    #[test]
    fn test_data_context_inference_productname() {
        let val = generate_test_value("ProductName");
        assert!(val.csharp_literal.contains("Test Product"));
    }
}
