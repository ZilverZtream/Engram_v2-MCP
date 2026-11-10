#![allow(clippy::unwrap_used)]
//! The VB sidecar must support incremental refresh.
//!
//! `begin_project` re-parses EVERY `.vb` file under the project root and
//! rebuilds the shared compilation — 7-18 s on a large solution. It ran on
//! every `index_files` call, so a watcher update that found `changed=1`
//! still paid the full cost (live 2026-08-20; the watcher fired every 13-25
//! seconds for 45 minutes and each pass did this).
//!
//! The fix is the `invalidate` command: drop just the cached trees for the
//! files about to be re-parsed. These tests drive the real sidecar binary
//! over its stdio protocol, because the risky part is the child's behaviour,
//! not the Rust wrapper.
//!
//! Skipped when the sidecar has not been published (`dotnet publish` in
//! tools/vb_roslyn_sidecar), so a machine without dotnet still runs green.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

fn sidecar_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("ENGRAM_VB_SIDECAR_PATH").filter(|path| !path.is_empty()) {
        let path = PathBuf::from(path);
        assert!(path.is_file(), "explicitly requested sidecar is missing: {}", path.display());
        return Some(path);
    }
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()?
        .parent()?
        .join("tools")
        .join("vb_roslyn_sidecar")
        .join("publish_out")
        .join(if cfg!(windows) {
            "vb_roslyn_sidecar.exe"
        } else {
            "vb_roslyn_sidecar"
        });
    p.exists().then_some(p)
}

struct Harness {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Harness {
    fn start(bin: &Path) -> Self {
        let mut child = Command::new(bin)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("sidecar must start");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
        }
    }

    fn send(&mut self, req: serde_json::Value) -> serde_json::Value {
        writeln!(self.stdin, "{req}").expect("write request");
        self.stdin.flush().expect("flush");
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read response");
        assert!(!line.trim().is_empty(), "sidecar returned an empty line");
        serde_json::from_str(&line).expect("response must be JSON")
    }

    fn symbol_names(&mut self, path: &Path, source: &str) -> Vec<String> {
        let resp = self.send(serde_json::json!({
            "cmd": "parse",
            "path": path.display().to_string(),
            "source": source,
        }));
        assert!(
            resp.get("error").is_none(),
            "parse failed: {}",
            resp.get("error").unwrap()
        );
        resp["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap_or_default().to_string())
            .collect()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

const SOURCE_V1: &str = "Class Widget\n  Public Sub Alpha()\n  End Sub\nEnd Class\n";
const SOURCE_V2: &str = "Class Widget\n  Public Sub Beta()\n  End Sub\nEnd Class\n";

/// Invocation syntax retains its ordinary call evidence without inventing a
/// dependency on an unrelated, uniquely named source property.
#[test]
fn unresolved_invocation_names_do_not_bind_unrelated_source_properties() {
    let Some(bin) = sidecar_path() else { return; };
    let owned = tempfile::TempDir::new().unwrap();
    let path = owned.path().join("invocation_property_collision.vb");
    let source = r#"Imports System
Imports System.Collections.Generic
Imports System.Linq
Public Class LayerScaleHint
    Public Property max As String
    Public Property Transform As String
End Class
Public Class CacheMeta
    Public Property CoordinateCount As Long
End Class
Public Class RowState
    Public Property Active As Boolean
    Public Property Reason As String
End Class
Public Class IndexedValues
    Default Public Property Item(index As Integer) As Long
        Get
            Return index
        End Get
        Set(value As Long)
        End Set
    End Property
End Class
Public Class OtherIndexedValues
    Default Public ReadOnly Property Item(index As Integer) As Long
        Get
            Return index
        End Get
    End Property
End Class
Public Class Consumers
    Private Class Band
        Public Property MaxFeatures As Long
        Public Property MaxCoordinates As Long
        Public Property MinZoom As Integer
    End Class
    Private Shared ReadOnly _bands As Band() = {
        New Band With {.MaxFeatures=100, .MaxCoordinates=2500, .MinZoom=10}}
    Public Function ClassifyAutomatic(featureCount As Long, coordinateCount As Long) As Integer
        For Each band In _bands
            If featureCount <= band.MaxFeatures AndAlso coordinateCount <= band.MaxCoordinates Then
                Return band.MinZoom
            End If
        Next
        Return 0
    End Function
    Public Function UnknownArray(values As Object()) As Long
        For Each value In values
            Return value.MaxFeatures
        Next
        Return 0
    End Function
    Public Function PredeclaredControl() As Long
        Dim value As MissingType
        For Each value In _bands
            Return value.MaxFeatures
        Next
        Return 0
    End Function
    Public Function ExplicitControl() As Long
        For Each value As MissingType In _bands
            Return value.MaxFeatures
        Next
        Return 0
    End Function
    Public Function Numeric(a As Integer, b As Integer) As Integer
        Return Math.Max(1, Math.Max(a, b))
    End Function
    Public Function Latest(updates As List(Of DateTime)) As DateTime
        Return updates.Max()
    End Function
    Public Function LatestImplicit(updates As List(Of DateTime)) As DateTime
        Return updates.Max
    End Function
    Public Function NextOrder(rows As List(Of Integer)) As Integer
        Dim existingOrders = rows.Select(Function(r) r).ToList()
        Return If(existingOrders.Any(), existingOrders.Max, 0) + 1
    End Function
    Public Function WithList(updates As List(Of DateTime)) As DateTime
        With updates
            Return .Max
        End With
    End Function
    Public Function WithCacheMeta(meta As CacheMeta) As Long
        With meta
            Return .CoordinateCount
        End With
    End Function
    Public Sub WriteInferredRow(rows As List(Of RowState))
        Dim row = rows.FirstOrDefault()
        With row
            .Active = True
            .Reason = "edited"
        End With
    End Sub
    Public Sub NestedIndexRead(rows As List(Of RowState), values As Long())
        Dim row = rows.FirstOrDefault()
        values(row.Active) = 0
    End Sub
    Public Function ReadInferredRow(rows As List(Of RowState)) As String
        Dim row = rows.FirstOrDefault()
        Return row.Reason
    End Function
    Public Sub NestedReceiverRead(rows As List(Of RowState))
        Dim row = rows.FirstOrDefault()
        row.Reason.Length = 1
    End Sub
    Private Function Transform(value As Integer) As Integer
        Return value
    End Function
    Private Function Transform(value As String) As Integer
        Return 0
    End Function
    Public Function BareImplicitMethod() As Integer
        Return Transform
    End Function
    Public Function MissingCall() As Object
        Return missing.Transform(1)
    End Function
    Public Function MissingIndexedCall() As Object
        Return missing.Item(0)
    End Function
    Public Function ReadBound(meta As CacheMeta) As Long
        Return meta.CoordinateCount
    End Function
    Public Function ReadIndexed(values As IndexedValues) As Long
        Return values.Item(0)
    End Function
    Public Sub WriteIndexed(values As IndexedValues, value As Long)
        values.Item(0) = value
    End Sub
    Public Function ReadUnresolved() As Long
        Return missing.CoordinateCount
    End Function
End Class
"#;
    std::fs::write(&path, source).unwrap();
    let mut harness = Harness::start(&bin);
    let response = harness.send(serde_json::json!({
        "cmd":"parse", "path":path, "source":source, "standalone":true,
    }));
    assert!(response.get("error").is_none(), "native parse failed: {response}");
    let edges = response["edges"].as_array().unwrap();
    for (caller, spelling, statement) in [
        ("Consumers.Numeric", "Math.Max", "Return Math.Max(1, Math.Max(a, b))"),
        ("Consumers.Latest", "updates.Max", "Return updates.Max()"),
        ("Consumers.MissingCall", "missing.Transform", "Return missing.Transform(1)"),
        ("Consumers.MissingIndexedCall", "missing.Item", "Return missing.Item(0)"),
    ] {
        assert!(!edges.iter().any(|edge| edge["source_name"] == caller
            && edge["kind"] == "calls" && edge["target_kind"] == "property"),
            "Invocation must not borrow a source property identity: {caller}: {edges:?}");
        let line = source.lines().position(|line| line.trim() == statement).unwrap() + 1;
        assert!(edges.iter().any(|edge| edge["source_name"] == caller
            && edge["kind"] == "calls" && edge["target_kind"] == "function"
            && edge["target_name"] == spelling && edge["metadata"]["unresolved"] == "true"
            && edge["metadata"]["call_site_line"] == line.to_string()),
            "The incomplete compilation must retain honest ordinary invocation text and position: {caller}");
    }
    for (caller, target, via, statement) in [
        ("Consumers.ReadBound", "CacheMeta.CoordinateCount", "property_get", "Return meta.CoordinateCount"),
        ("Consumers.WithCacheMeta", "CacheMeta.CoordinateCount", "property_get", "Return .CoordinateCount"),
        ("Consumers.ReadIndexed", "IndexedValues.Item", "property_get", "Return values.Item(0)"),
        ("Consumers.WriteIndexed", "IndexedValues.Item", "property_set", "values.Item(0) = value"),
        ("Consumers.ReadUnresolved", "CacheMeta.CoordinateCount", "property_get", "Return missing.CoordinateCount"),
        ("Consumers.WriteInferredRow", "RowState.Active", "property_set", ".Active = True"),
        ("Consumers.WriteInferredRow", "RowState.Reason", "property_set", ".Reason = \"edited\""),
        ("Consumers.ClassifyAutomatic", "Consumers.Band.MaxFeatures", "property_get", "If featureCount <= band.MaxFeatures AndAlso coordinateCount <= band.MaxCoordinates Then"),
        ("Consumers.ClassifyAutomatic", "Consumers.Band.MaxCoordinates", "property_get", "If featureCount <= band.MaxFeatures AndAlso coordinateCount <= band.MaxCoordinates Then"),
        ("Consumers.ClassifyAutomatic", "Consumers.Band.MinZoom", "property_get", "Return band.MinZoom"),
    ] {
        let line = source.lines().position(|line| line.trim() == statement).unwrap() + 1;
        assert!(edges.iter().any(|edge| edge["source_name"] == caller
            && edge["kind"] == "calls" && edge["target_kind"] == "property"
            && edge["target_name"] == target && edge["metadata"]["via"] == via
            && edge["metadata"]["call_site_line"] == line.to_string()),
            "Bound property/indexer and existing noninvocation fallback must survive: {caller}: {edges:?}");
    }
    for (caller, target) in [
        ("Consumers.LatestImplicit", "LayerScaleHint.max"),
        ("Consumers.NextOrder", "LayerScaleHint.max"),
        ("Consumers.WithList", "LayerScaleHint.max"),
        ("Consumers.BareImplicitMethod", "LayerScaleHint.Transform"),
        ("Consumers.UnknownArray", "Consumers.Band.MaxFeatures"),
        ("Consumers.PredeclaredControl", "Consumers.Band.MaxFeatures"),
        ("Consumers.ExplicitControl", "Consumers.Band.MaxFeatures"),
        ("Consumers.NestedIndexRead", "RowState.Active"),
        ("Consumers.ReadInferredRow", "RowState.Reason"),
        ("Consumers.NestedReceiverRead", "RowState.Reason"),
    ] {
        assert!(!edges.iter().any(|edge| edge["source_name"] == caller
            && edge["kind"] == "calls" && edge["target_kind"] == "property"
            && edge["target_name"] == target),
            "An implicit collection method must not borrow an unrelated source property: {caller}: {edges:?}");
    }
    harness.send(serde_json::json!({"cmd":"shutdown"}));
    assert!(harness.child.wait().unwrap().success());
    drop(harness);
    owned.close().unwrap();
}

/// A scalar with the same spelling as a source property must not become a
/// property reader/writer in the native producer's dependency graph.
#[test]
fn resolved_non_property_symbols_do_not_bind_same_named_source_properties() {
    let Some(bin) = sidecar_path() else { return; };
    // Declaration order also closes the child before removing the fixture on panic.
    let owned = tempfile::TempDir::new().unwrap();
    let path = owned.path().join("property_shadowing.vb");
    let source = r#"Public Class CacheMeta
    Public Property CoordinateCount As Long
End Class
Public Class ScalarInputs
    Public Function ClassifyAutomatic(coordinateCount As Long) As Long
        Return coordinateCount
    End Function
    Public Function ReadLocal() As Long
        Dim coordinateCount As Long = 7
        Return coordinateCount
    End Function
    Public Function ReadProperty(meta As CacheMeta) As Long
        Return meta.CoordinateCount
    End Function
    Public Sub WriteProperty(meta As CacheMeta, value As Long)
        meta.CoordinateCount = value
    End Sub
End Class
Public Class FieldInputs
    Private coordinateCount As Long
    Public Function ReadField() As Long
        Return coordinateCount
    End Function
    Public Function ReadQualifiedField() As Long
        Return Me.coordinateCount
    End Function
    Public Sub WriteField(value As Long)
        Me.coordinateCount = value
    End Sub
End Class
Public Class MethodInputs
    Private Function CoordinateCount() As Long
        Return 3
    End Function
    Public Function ReadMethod() As Long
        Return CoordinateCount()
    End Function
End Class
Public Class BarePropertyInputs
    Public Property Count As Long
    Public Function ReadBare() As Long
        Return Count
    End Function
    Public Sub WriteBare(value As Long)
        Count = value
    End Sub
    Public Function ReadShadowed(count As Long) As Long
        Return count
    End Function
End Class
Public Class LegacyInputs
    Public Function ReadUnresolved() As Long
        Return missing.CoordinateCount
    End Function
End Class
"#;
    std::fs::write(&path, source).unwrap();
    let mut harness = Harness::start(&bin);
    let response = harness.send(serde_json::json!({
        "cmd": "parse", "path": path, "source": source, "standalone": true,
    }));
    assert!(response.get("error").is_none(), "native parse failed: {response}");
    assert_eq!(response["parser_identity"]["contract"], "vb-ingestion-parser-v6");
    let edges = response["edges"].as_array().unwrap();
    let property_edges: Vec<_> = edges.iter().filter(|edge|
        edge["kind"] == "calls" && edge["target_kind"] == "property").collect();
    for caller in [
        "ScalarInputs.ClassifyAutomatic", "ScalarInputs.ReadLocal",
        "FieldInputs.ReadField", "FieldInputs.ReadQualifiedField", "FieldInputs.WriteField",
        "MethodInputs.ReadMethod", "BarePropertyInputs.ReadShadowed",
    ] {
        assert!(!property_edges.iter().any(|edge| edge["source_name"] == caller),
            "resolved scalar/method must not become a property dependency: {caller}: {property_edges:?}");
    }
    for (caller, target, via, statement) in [
        ("ScalarInputs.ReadProperty", "CacheMeta.CoordinateCount", "property_get", "Return meta.CoordinateCount"),
        ("ScalarInputs.WriteProperty", "CacheMeta.CoordinateCount", "property_set", "meta.CoordinateCount = value"),
        ("BarePropertyInputs.ReadBare", "BarePropertyInputs.Count", "property_get", "Return Count"),
        ("BarePropertyInputs.WriteBare", "BarePropertyInputs.Count", "property_set", "Count = value"),
        ("LegacyInputs.ReadUnresolved", "CacheMeta.CoordinateCount", "property_get", "Return missing.CoordinateCount"),
    ] {
        let matches: Vec<_> = property_edges.iter().filter(|edge|
            edge["source_name"] == caller && edge["target_name"] == target
                && edge["metadata"]["via"] == via).collect();
        assert_eq!(matches.len(), 1, "retain the existing property read/write: {caller}: {property_edges:?}");
        let line = source.lines().position(|line| line.trim() == statement).unwrap() + 1;
        assert_eq!(matches[0]["metadata"]["call_site_line"], line.to_string(),
            "property navigation must retain its actual source witness");
    }
    assert!(edges.iter().any(|edge| edge["kind"] == "calls"
        && edge["source_name"] == "MethodInputs.ReadMethod"
        && edge["target_name"] == "MethodInputs.CoordinateCount"
        && edge["target_kind"] == "function"), "the real method call must remain");
    harness.send(serde_json::json!({"cmd": "shutdown"}));
    assert!(harness.child.wait().unwrap().success());
    drop(harness);
    owned.close().unwrap();
}

/// The project compiler environment must resolve the implicit LINQ receiver
/// before choosing among identically named source and projection properties.
#[cfg(windows)]
#[test]
fn configured_framework_context_resolves_projection_rhs_and_fences_config_changes() {
    let Some(bin) = sidecar_path() else { return; };
    // Declare the directory first: Harness joins its owned child before the
    // directory is removed, including when an assertion unwinds the fixture.
    let owned = tempfile::TempDir::new().unwrap();
    let root = owned.path();
    let config = root.join("web.config");
    let configuration = r#"<configuration>
<system.web><compilation targetFramework="4.8" defaultLanguage="vb" debug="true"><assemblies>
<add assembly="System.Data.Linq, Version=4.0.0.0, Culture=neutral, PublicKeyToken=b77a5c561934e089"/>
</assemblies></compilation><pages><namespaces><clear/>
<add namespace="System"/><add namespace="System.Collections.Generic"/><add namespace="System.Linq"/>
</namespaces></pages></system.web>
<system.codedom><compilers><compiler language="vb;vbs;visualbasic;vbscript" extension=".vb"
type="Microsoft.CodeDom.Providers.DotNetCompilerPlatform.VBCodeProvider, Microsoft.CodeDom.Providers.DotNetCompilerPlatform, Version=4.1.0.0, Culture=neutral, PublicKeyToken=31bf3856ad364e35"
compilerOptions="/optioninfer+"><providerOption name="UseAspNetSettings" value="true"/>
</compiler></compilers></system.codedom></configuration>"#;
    std::fs::write(&config, configuration).unwrap();
    let source = r#"Namespace DataModel
Public Class Entry
    Public Property EntityId As Integer
End Class
End Namespace
Namespace DisplayModel
Public Class Entry
    Public Property EntityId As Integer
End Class
End Namespace
Public Class Projector
#If DEBUG Then
#If _MYTYPE = "Web" Then
    Public Function Build(rows As List(Of DataModel.Entry)) As List(Of DisplayModel.Entry)
        Dim selected = From row In rows Select New DisplayModel.Entry With {
            .EntityId = _
                row.EntityId}
        Return selected.ToList()
    End Function
#End If
#End If
End Class
"#;
    let path = root.join("Projection.vb");
    std::fs::write(&path, source).unwrap();
    let mut harness = Harness::start(&bin);
    let context_reply = harness.send(serde_json::json!({"cmd":"compiler_context", "project_root":root}));
    assert!(context_reply.get("error").is_none(), "compiler context: {context_reply}");
    assert_eq!(context_reply["parser_identity"]["contract"], "vb-ingestion-parser-v6");
    let context = context_reply["compiler_context"].clone();
    assert_eq!(context["mode"], "framework_only",
        "requires installed .NET Framework 4.8 references; source-only cannot prove LINQ semantics: {context}");
    let references = context["reference_bytes"].as_u64().unwrap();
    assert!(references > 0 && references <= 64 * 1024 * 1024);
    let policy = serde_json::json!({"max_total_source_bytes":1048576,"max_file_source_bytes":524288,
        "max_trees":64,"allocation_multiplier":32,"tree_overhead_bytes":8192});
    let plan = harness.send(serde_json::json!({"cmd":"plan_refresh", "project_root":root,
        "files":[path], "paths":[path], "compilation_policy":policy,"compiler_context":context}));
    assert!(plan.get("error").is_none(), "plan: {plan}");
    assert_eq!(plan["compiler_context"], context);
    assert_eq!(plan["compilation_plan"]["compiler_context"], context);
    assert_eq!(plan["compilation_plan"]["reference_bytes"], references,
        "native planning must expose framework bytes before the owner admits reference allocation");
    let prepared = harness.send(serde_json::json!({"cmd":"refresh_project", "plan_id":plan["compilation_plan"]["id"]}));
    assert!(prepared.get("error").is_none(), "refresh: {prepared}");
    assert_eq!(prepared["compiler_context"], context);
    let parsed = harness.send(serde_json::json!({"cmd":"parse", "project_root":root,
        "path":path,"source":source,"compiler_context":context}));
    assert!(parsed.get("error").is_none(), "parse: {parsed}");
    assert_eq!(parsed["compiler_context"], context);
    // Put the initializer destination and RHS on separate source lines so the
    // assertion identifies the receiver read without forbidding the DTO write.
    let witness = source.lines().position(|line| line.trim() == "row.EntityId}").unwrap() + 1;
    let reads: Vec<_> = parsed["edges"].as_array().unwrap().iter().filter(|edge|
        edge["kind"] == "calls" && edge["source_name"] == "Projector.Build"
            && edge["target_kind"] == "property" && edge["metadata"]["via"] == "property_get"
            && edge["metadata"]["call_site_line"] == witness.to_string()).collect();
    assert_eq!(reads.len(), 1, "the projection RHS is one source-property read: {reads:?}");
    assert_eq!(reads[0]["target_name"], "DataModel.Entry.EntityId",
        "the DTO initializer's same-named target must not steal the range variable's RHS");
    assert_eq!(reads[0]["metadata"]["call_site_line"], witness.to_string());
    assert!(reads[0]["metadata"].get("unresolved").is_none());

    // The source and inventory remain byte-identical. Removing a global
    // import between plan and refresh must invalidate that admitted plan.
    let warm_plan = harness.send(serde_json::json!({"cmd":"plan_refresh", "project_root":root,
        "paths":[], "compilation_policy":policy,"compiler_context":context}));
    assert!(warm_plan.get("error").is_none(), "warm plan: {warm_plan}");
    assert_eq!(warm_plan["compilation_plan"]["full"], false);
    assert!(warm_plan["compilation_plan"]["id"].as_u64().is_some_and(|id| id > 0));
    std::fs::write(&config, configuration.replace("<add namespace=\"System.Linq\"/>", "<remove namespace=\"System.Linq\"/>")).unwrap();
    let refused = harness.send(serde_json::json!({"cmd":"refresh_project", "plan_id":warm_plan["compilation_plan"]["id"]}));
    assert_eq!(refused["compilation_error"], true, "a changed compiler input cannot reuse its old plan: {refused}");
    assert!(refused["error"].as_str().is_some_and(|error| error.contains("Compiler context changed after planning")),
        "The valid plan must be rejected for the compiler-input mutation: {refused}");
    let changed = harness.send(serde_json::json!({"cmd":"compiler_context", "project_root":root}));
    assert!(changed.get("error").is_none(), "changed context: {changed}");
    assert_eq!(changed["compiler_context"]["mode"], "framework_only");
    assert_ne!(changed["compiler_context"]["fingerprint"], context["fingerprint"]);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
    let stale = harness.send(serde_json::json!({"cmd":"parse", "project_root":root,
        "path":path,"source":source,"compiler_context":context}));
    assert!(stale.get("error").is_some(), "old compiler authority must not parse after the rejected refresh: {stale}");
    harness.send(serde_json::json!({"cmd":"shutdown"}));
    assert!(harness.child.wait().unwrap().success());
    drop(harness);
    owned.close().unwrap();
}

/// Contract callers must remain discoverable without confusing overloads,
/// namespaces or constructed generic interfaces. Drive the actual native
/// producer, rather than manufacturing the metadata expected by ingestion.
#[test]
fn interface_dispatch_preserves_exact_declarations_and_constructed_contracts() {
    let Some(bin) = sidecar_path() else { return; };
    let owned = tempfile::TempDir::new().unwrap();
    let contract = "Namespace Example\nPublic Class Alpha\nEnd Class\nPublic Class Beta\nEnd Class\nPublic Interface IReader\n Function Read(value As Alpha) As Alpha\n Function Read(value As Beta) As Beta\nEnd Interface\nPublic Interface IBox(Of T)\n Function Read(value As T) As T\nEnd Interface\nEnd Namespace\nNamespace Other\nPublic Interface IReader\n Function Read(value As Example.Alpha) As Example.Alpha\nEnd Interface\nEnd Namespace\n";
    let implementation = "Namespace Example\nPublic Class Reader\n Implements IReader, IBox(Of Alpha), IBox(Of Beta)\n Public Function Fetch(value As Alpha) As Alpha Implements IReader.Read, IBox(Of Alpha).Read\n Return value\n End Function\n Public Function Fetch(value As Beta) As Beta Implements IReader.Read, IBox(Of Beta).Read\n Return value\n End Function\nEnd Class\nPublic Class OpenReader(Of T)\n Implements IBox(Of T)\n Public Function Read(value As T) As T Implements IBox(Of T).Read\n Return value\n End Function\nEnd Class\nPublic Class MissingReader\n Public Sub Work() Implements MissingContract.Work\n End Sub\nEnd Class\nEnd Namespace\n";
    let caller = "Namespace Example\nPublic Class Controller\n Public Function First(service As IReader, value As Alpha) As Alpha\n Return service.Read(value)\n End Function\n Public Function Second(service As IReader, value As Beta) As Beta\n Return service.Read(value)\n End Function\n Public Function BoxA(service As IBox(Of Alpha), value As Alpha) As Alpha\n Return service.Read(value)\n End Function\n Public Function BoxB(service As IBox(Of Beta), value As Beta) As Beta\n Return service.Read(value)\n End Function\n Public Function Other(service As Other.IReader, value As Alpha) As Alpha\n Return service.Read(value)\n End Function\nEnd Class\nEnd Namespace\n";
    let caller = caller.replace(" Public Function Other(", " Public Function Ambiguous(service As IReader) As Alpha\n Return service.Read(Nothing)\n End Function\n Public Function Other(");
    let sources = [("contracts.vb", contract), ("implementation.vb", implementation), ("caller.vb", caller.as_str())];
    let paths: Vec<_> = sources.iter().map(|(name, source)| {
        let path = owned.path().join(name);
        std::fs::write(&path, source).unwrap();
        path.display().to_string()
    }).collect();
    let mut harness = Harness::start(&bin);
    let plan = harness.send(serde_json::json!({"cmd":"plan_refresh", "project_root":owned.path(),
        "files":paths, "paths":paths, "compilation_policy":{
            "max_total_source_bytes":1048576, "max_file_source_bytes":524288,
            "max_trees":64, "allocation_multiplier":32, "tree_overhead_bytes":8192}}));
    assert!(plan.get("error").is_none(), "plan: {plan}");
    let prepared = harness.send(serde_json::json!({"cmd":"refresh_project", "plan_id":plan["compilation_plan"]["id"]}));
    assert!(prepared.get("error").is_none(), "refresh: {prepared}");
    let parsed: Vec<_> = sources.iter().zip(&paths).map(|((_, source), path)| {
        let result = harness.send(serde_json::json!({"cmd":"parse", "project_root":owned.path(), "path":path,"source":source}));
        assert!(result.get("error").is_none(), "parse: {result}");
        result
    }).collect();
    let declarations: Vec<_> = parsed[0]["symbols"].as_array().unwrap().iter()
        .filter(|node| node["metadata"]["interface_member_declaration"] == "true").collect();
    assert_eq!(declarations.len(), 4, "all contract methods, including overloads, must be navigable");
    assert!(!parsed[0]["edges"].as_array().unwrap().iter().any(|edge| edge["kind"] == "calls"),
        "a declaration has no implementation body");
    let bindings = |response: &serde_json::Value, key: &str| -> Vec<serde_json::Value> {
        response["edges"].as_array().unwrap().iter().filter_map(|edge| {
            edge["metadata"][key].as_str().map(|value| serde_json::from_str(value).unwrap())
        }).collect()
    };
    let mappings = bindings(&parsed[1], "interface_member_mapping_v1");
    let calls = bindings(&parsed[2], "interface_call_target_v1");
    assert_eq!(mappings.len(), 4, "two exact overloads and two closed generic implementations");
    assert_eq!(calls.len(), 5, "all statically resolved interface calls have exact locators");
    assert!(parsed[2]["edges"].as_array().unwrap().iter().any(|edge|
        edge["metadata"]["interface_call_status"] == "unknown"
            && edge["metadata"].get("interface_call_target_v1").is_none()),
        "An ambiguous interface invocation must not fall through to generic name/arity resolution");
    for locator in mappings.iter().chain(&calls) {
        assert_eq!(locator["file_path"], "contracts.vb");
        assert_eq!(locator["syntax_source_sha256"], parsed[0]["symbols"][0]["metadata"]["vb_syntax_source_sha256"]);
        let matches: Vec<_> = declarations.iter().filter(|declaration| {
            declaration["name"] == locator["name"] && declaration["start_line"] == locator["start_line"]
                && declaration["metadata"]["semantic_member_id"] == locator["semantic_id"]
                && declaration["metadata"]["semantic_span_start"].as_str().unwrap().parse::<u64>().unwrap() == locator["span_start"].as_u64().unwrap()
        }).collect();
        assert_eq!(matches.len(), 1, "exact declared target must exist: {locator}");
    }
    assert_ne!(calls[0]["semantic_id"], calls[1]["semantic_id"], "same-name/arity overloads stay distinct");
    assert_eq!(calls[2]["semantic_id"], calls[3]["semantic_id"]);
    assert_ne!(calls[2]["contract_type_key"], calls[3]["contract_type_key"], "closed generic instances must not cross-bind");
    assert_ne!(calls[0]["semantic_id"], calls[4]["semantic_id"], "same short names in other namespaces stay distinct");
    for name in ["Example.OpenReader.Read", "Example.MissingReader.Work"] {
        let method = parsed[1]["symbols"].as_array().unwrap().iter().find(|node| node["name"] == name).unwrap();
        assert_eq!(method["metadata"]["interface_dispatch_status"], "unknown", "unsupported dispatch must stay open");
    }
    harness.send(serde_json::json!({"cmd":"shutdown"}));
    assert!(harness.child.wait().unwrap().success());
    drop(harness); // Close child handles before the owned source directory.
}

#[test]
fn statically_typed_receiver_retains_its_verified_declaring_type() {
    let Some(bin) = sidecar_path() else { return; };
    let mut harness = Harness::start(&bin);
    let response = harness.send(serde_json::json!({"cmd":"parse", "path":"receiver.vb",
        "source":"Namespace Example\nPublic Class Store\n Public Function Exists() As Boolean\n Return True\n End Function\nEnd Class\nPublic Class Caller\n Public Sub Check(store As Store)\n If store.Exists() Then Return\n End Sub\nEnd Class\nEnd Namespace"}));
    let calls: Vec<_> = response["edges"].as_array().unwrap().iter().filter(|e| e["kind"] == "calls").collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["target_name"], "Example.Store.Exists");
    assert!(calls[0]["metadata"].get("unresolved").is_none());
    assert_eq!(calls[0]["metadata"]["source_call_status"], "unknown",
        "an unadmitted standalone source target must not enter the name/arity resolver");
    assert!(calls[0]["metadata"].get("source_call_target_v1").is_none());
}

/// Both logger overloads accept five arguments. Compiler selection, source
/// navigation and the call-site witness must survive either declaration order.
#[cfg(windows)]
#[test]
fn ordinary_source_calls_preserve_compiler_selected_optional_overloads() {
    use sha2::{Digest, Sha256};
    let Some(bin) = sidecar_path() else { return; };
    let boolean = "Public Shared Sub Create(m As String, f As String, e As String, ok As Boolean, current As Boolean, Optional message As String = \"\", Optional extra As String = \"\")\nEnd Sub\n";
    let guid = "Public Shared Sub Create(m As String, f As String, e As String, ok As Boolean, user As System.Guid, Optional message As String = \"\")\nEnd Sub\n";
    for reversed in [false, true] {
        let owned = tempfile::TempDir::new().unwrap();
        std::fs::write(owned.path().join("web.config"),
            r#"<configuration>
<system.web><compilation targetFramework="4.8" defaultLanguage="vb" debug="true"><assemblies>
<add assembly="System.Data.Linq, Version=4.0.0.0, Culture=neutral, PublicKeyToken=b77a5c561934e089"/>
</assemblies></compilation><pages><namespaces><clear/>
<add namespace="System"/><add namespace="System.Collections.Generic"/><add namespace="System.Linq"/>
</namespaces></pages></system.web>
<system.codedom><compilers><compiler language="vb;vbs;visualbasic;vbscript" extension=".vb"
type="Microsoft.CodeDom.Providers.DotNetCompilerPlatform.VBCodeProvider, Microsoft.CodeDom.Providers.DotNetCompilerPlatform, Version=4.1.0.0, Culture=neutral, PublicKeyToken=31bf3856ad364e35"
compilerOptions="/optioninfer+"><providerOption name="UseAspNetSettings" value="true"/>
</compiler></compilers></system.codedom></configuration>"#).unwrap();
        let methods = if reversed { format!("{guid}{boolean}") } else { format!("{boolean}{guid}") };
        let logger = format!("' Unicode before the target: \u{1f4cd}\nNamespace Sample\nPublic Class AuditLog\n{methods}End Class\nEnd Namespace\n");
        let mut caller = String::from("Public Interface IContract\n Sub Run()\nEnd Interface\nPublic Class Controller\n Public Sub DeleteLayers()\n Sample.AuditLog.Create(\"map\", \"DeleteLayers\", \"deleted\", True, True)\n End Sub\n Public Sub DeleteForUser(user As System.Guid)\n Sample.AuditLog.Create(\"map\", \"DeleteForUser\", \"deleted\", True, user)\n End Sub\n Public Sub Contract(service As IContract)\n service.Run()\n End Sub\n Public Sub ExternalAndMissing()\n System.Console.WriteLine(\"external\")\n Missing.Work()\n End Sub\n Public Sub Many()\n");
        for _ in 0..257 {
            caller.push_str(" Sample.AuditLog.Create(\"map\", \"Many\", \"deleted\", True, True)\n");
        }
        caller.push_str(" End Sub\nEnd Class\n");
        let sources = [("logger.vb", logger.as_str()), ("caller.vb", caller.as_str())];
        let paths: Vec<_> = sources.iter().map(|(name, source)| {
            let path = owned.path().join(name);
            std::fs::write(&path, source).unwrap();
            path
        }).collect();
        let mut harness = Harness::start(&bin);
        let reply = harness.send(serde_json::json!({"cmd":"compiler_context","project_root":owned.path()}));
        assert!(reply.get("error").is_none(), "context: {reply}");
        let context = reply["compiler_context"].clone();
        assert_eq!(context["mode"], "framework_only", "real System.Guid overload requires framework references");
        let plan = harness.send(serde_json::json!({"cmd":"plan_refresh","project_root":owned.path(),
            "files":paths,"paths":paths,"compiler_context":context,"compilation_policy":{
                "max_total_source_bytes":1048576,"max_file_source_bytes":524288,
                "max_trees":64,"allocation_multiplier":32,"tree_overhead_bytes":8192}}));
        assert!(plan.get("error").is_none(), "plan: {plan}");
        let prepared = harness.send(serde_json::json!({"cmd":"refresh_project","plan_id":plan["compilation_plan"]["id"]}));
        assert!(prepared.get("error").is_none(), "refresh: {prepared}");
        let parsed: Vec<_> = sources.iter().zip(&paths).map(|((_, source), path)| {
            let response = harness.send(serde_json::json!({"cmd":"parse","project_root":owned.path(),
                "path":path,"source":source,"compiler_context":context}));
            assert!(response.get("error").is_none(), "parse: {response}");
            response
        }).collect();
        let edges = parsed[1]["edges"].as_array().unwrap();
        for (method, declaration, signature) in [
            ("Controller.DeleteLayers", boolean, "M:Sample.AuditLog.Create(System.String,System.String,System.String,System.Boolean,System.Boolean,System.String,System.String)"),
            ("Controller.DeleteForUser", guid, "M:Sample.AuditLog.Create(System.String,System.String,System.String,System.Boolean,System.Guid,System.String)"),
        ] {
            let edge = edges.iter().find(|edge| edge["kind"] == "calls" && edge["source_name"] == method).unwrap();
            assert_eq!(edge["target_name"], "Sample.AuditLog.Create");
            assert_eq!(edge["metadata"]["args"], "5");
            assert_eq!(edge["metadata"]["source_call_status"], "source_located");
            assert!(edge.get("target_start_line").is_none_or(serde_json::Value::is_null),
                "a cross-file source target must not use the same-file line shortcut");
            let locator: serde_json::Value = serde_json::from_str(edge["metadata"]["source_call_target_v1"].as_str().unwrap()).unwrap();
            assert_eq!(locator["version"], 1);
            assert_eq!(locator["name"], edge["target_name"]);
            assert_eq!(locator["file_path"], "logger.vb");
            assert_eq!(locator["semantic_id"], signature);
            assert_eq!(locator["syntax_source_sha256"], format!("{:x}", Sha256::digest(logger.as_bytes())));
            let declaration_line = logger.lines().position(|line| line == declaration.lines().next().unwrap()).unwrap() + 1;
            assert_eq!(locator["start_line"], declaration_line);
            let units: Vec<_> = logger.encode_utf16().collect();
            let start = locator["span_start"].as_u64().unwrap() as usize;
            let length = locator["span_length"].as_u64().unwrap() as usize;
            assert_eq!(String::from_utf16(&units[start..start + length]).unwrap(), declaration.lines().next().unwrap());
            let matching: Vec<_> = parsed[0]["symbols"].as_array().unwrap().iter().filter(|symbol|
                symbol["name"] == locator["name"] && symbol["start_line"] == locator["start_line"]
                    && symbol["metadata"]["semantic_member_id"] == locator["semantic_id"]).collect();
            assert_eq!(matching.len(), 1, "navigation must identify exactly the selected source declaration");
            assert_eq!(matching[0]["metadata"]["arity_min"], "5");
            let witness = edge["metadata"]["call_site_line"].as_str().unwrap().parse::<usize>().unwrap();
            let expected = if method.ends_with("DeleteLayers") { "\"DeleteLayers\", \"deleted\", True, True)" }
                else { "\"DeleteForUser\", \"deleted\", True, user)" };
            assert!(caller.lines().nth(witness - 1).unwrap().contains(expected));
        }
        let contract = edges.iter().find(|edge| edge["kind"] == "calls" && edge["source_name"] == "Controller.Contract").unwrap();
        assert_eq!(contract["metadata"]["interface_call_status"], "source_located");
        assert!(contract["metadata"].get("source_call_status").is_none(), "interface dispatch remains a separate route");
        let ordinary: Vec<_> = edges.iter().filter(|edge| edge["target_name"] == "Sample.AuditLog.Create" && edge["kind"] == "calls").collect();
        assert_eq!(ordinary.len(), 259);
        assert_eq!(ordinary.iter().filter(|edge| edge["metadata"]["source_call_status"] == "source_located").count(), 256);
        let omitted: Vec<_> = ordinary.iter().filter(|edge| edge["metadata"]["source_call_status"] == "unknown").collect();
        assert_eq!(omitted.len(), 3, "exhausted source locators must stay unresolved rather than guessing an overload");
        assert!(omitted.iter().all(|edge| edge["metadata"].get("source_call_target_v1").is_none()));
        let others: Vec<_> = edges.iter().filter(|edge| edge["kind"] == "calls" && edge["source_name"] == "Controller.ExternalAndMissing").collect();
        assert_eq!(others.len(), 2);
        assert!(others.iter().all(|edge| edge["metadata"].get("source_call_status").is_none()),
            "external and unbound calls retain ordinary behavior");
        harness.send(serde_json::json!({"cmd":"shutdown"}));
        assert!(harness.child.wait().unwrap().success());
        drop(harness);
        owned.close().unwrap();
    }
}

#[test]
fn method_metadata_includes_signature_access_and_return_type() {
    let Some(bin) = sidecar_path() else {
        return;
    };
    let mut harness = Harness::start(&bin);
    let response = harness.send(serde_json::json!({"cmd":"parse", "path":"batch.vb",
        "source":"Public Class Batch\n Public Shared Function CreateMany(id As Integer) As Boolean\n Return True\n End Function\nEnd Class"}));
    let method = response["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| {
            s["name"]
                .as_str()
                .unwrap_or_default()
                .ends_with("CreateMany")
        })
        .unwrap();
    let meta = &method["metadata"];
    assert_eq!(meta["access_level"], "Public");
    assert_eq!(meta["return_type"], "Boolean");
    assert!(
        meta["signature"]
            .as_str()
            .unwrap()
            .contains("Public Shared Function CreateMany(id As Integer) As Boolean")
    );
}

/// A warmed project must never silently substitute older source bytes. Legacy
/// unscoped parses use requested bytes; prepared ingestion refuses a mismatch.
#[test]
fn changed_requests_never_silently_return_warmed_source_bytes() {
    let Some(bin) = sidecar_path() else {
        eprintln!("sidecar not published — skipping");
        return;
    };
    let tmp = tempfile::TempDir::new().unwrap();
    let file = tmp.path().join("widget.vb");
    std::fs::write(&file, SOURCE_V1).unwrap();

    let mut h = Harness::start(&bin);
    let begun = h.send(serde_json::json!({
        "cmd": "begin_project",
        "project_root": tmp.path().display().to_string(),
    }));
    assert!(begun.get("error").is_none(), "begin_project: {begun}");

    let fresh = h.symbol_names(&file, SOURCE_V2);
    assert!(
        fresh.iter().any(|n| n.ends_with("Beta")),
        "requested bytes must win over the warmed source; got {fresh:?}"
    );
    assert!(
        !fresh.iter().any(|n| n.ends_with("Alpha")),
        "stale declarations must not accompany the requested source; got {fresh:?}"
    );
    let refused=h.send(serde_json::json!({"cmd":"parse","project_root":tmp.path(),"path":file,"source":SOURCE_V2}));
    assert_eq!(refused["compilation_error"], true, "prepared ingestion must refuse mismatched bytes: {refused}");
    assert!(refused["symbols"].as_array().is_none_or(Vec::is_empty), "a refused parse must not return declarations");
    let baseline=h.symbol_names(&file,SOURCE_V1);
    assert!(baseline.iter().any(|name|name.ends_with("Alpha")),"a one-off query must preserve the prepared baseline");

    let dropped = h.send(serde_json::json!({
        "cmd": "invalidate",
        "paths": [file.display().to_string()],
    }));
    assert!(dropped.get("error").is_none(), "invalidate: {dropped}");
    assert_eq!(
        dropped["invalidated"].as_i64(),
        Some(1),
        "invalidate must report the tree it dropped: {dropped}"
    );

    let fresh = h.symbol_names(&file, SOURCE_V2);
    assert!(
        fresh.iter().any(|n| n.ends_with("Beta")),
        "after invalidate the caller's source must be used; got {fresh:?}"
    );
    h.send(serde_json::json!({"cmd":"shutdown"}));
    assert!(h.child.wait().unwrap().success());
}

/// Invalidating one file must not tear down the rest of the project
/// compilation — otherwise the "incremental" path would silently degrade
/// every other file to single-file parsing.
#[test]
fn invalidate_keeps_the_rest_of_the_project_warm() {
    let Some(bin) = sidecar_path() else {
        eprintln!("sidecar not published — skipping");
        return;
    };
    let tmp = tempfile::TempDir::new().unwrap();
    let a = tmp.path().join("a.vb");
    let b = tmp.path().join("b.vb");
    std::fs::write(&a, SOURCE_V1).unwrap();
    std::fs::write(
        &b,
        "Public Interface IContract\n Function Read() As Integer\nEnd Interface\n",
    )
    .unwrap();

    let mut h = Harness::start(&bin);
    h.send(serde_json::json!({
        "cmd": "begin_project",
        "project_root": tmp.path().display().to_string(),
    }));

    let dropped = h.send(serde_json::json!({
        "cmd": "invalidate",
        "paths": [a.display().to_string()],
    }));
    assert_eq!(dropped["invalidated"].as_i64(), Some(1));

    // A caller in the invalidated file must still bind an interface declared
    // only in the untouched peer. No stale request bytes are needed to test it.
    let parsed=h.send(serde_json::json!({"cmd":"parse","path":a,
        "source":"Public Class Widget\n Public Function Read(service As IContract) As Integer\n Return service.Read()\n End Function\nEnd Class\n"}));
    let call=parsed["edges"].as_array().unwrap().iter().find(|edge|edge["kind"]=="calls").unwrap();
    assert_eq!(call["target_name"],"IContract.Read");
    let locator:serde_json::Value=serde_json::from_str(call["metadata"]["interface_call_target_v1"].as_str().unwrap()).unwrap();
    assert_eq!(locator["file_path"],"b.vb","untouched peer declaration must stay available to semantic binding");
    let remaining=h.send(serde_json::json!({"cmd":"invalidate","paths":[b]}));
    assert_eq!(remaining["invalidated"],1);
    h.send(serde_json::json!({"cmd":"shutdown"}));
    assert!(h.child.wait().unwrap().success());
}

/// Invalidating a path the sidecar never cached is a no-op, not an error —
/// the Rust side passes changed files without checking membership.
#[test]
fn invalidating_unknown_paths_is_a_no_op() {
    let Some(bin) = sidecar_path() else {
        eprintln!("sidecar not published — skipping");
        return;
    };
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("a.vb"), SOURCE_V1).unwrap();

    let mut h = Harness::start(&bin);
    h.send(serde_json::json!({
        "cmd": "begin_project",
        "project_root": tmp.path().display().to_string(),
    }));

    let resp = h.send(serde_json::json!({
        "cmd": "invalidate",
        "paths": [tmp.path().join("never_seen.vb").display().to_string()],
    }));
    assert!(resp.get("error").is_none(), "must not error: {resp}");
    assert_eq!(resp["invalidated"].as_i64(), Some(0));
}

/// The project walk must skip build output. A generated copy under obj\
/// would otherwise be parsed into the shared compilation and duplicate every
/// type it declares.
#[test]
fn begin_project_skips_build_output_directories() {
    let Some(bin) = sidecar_path() else {
        eprintln!("sidecar not published — skipping");
        return;
    };
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("real.vb"), SOURCE_V1).unwrap();
    let obj = tmp.path().join("obj").join("Debug");
    std::fs::create_dir_all(&obj).unwrap();
    let generated = obj.join("generated.vb");
    std::fs::write(&generated, SOURCE_V2).unwrap();

    let mut h = Harness::start(&bin);
    h.send(serde_json::json!({
        "cmd": "begin_project",
        "project_root": tmp.path().display().to_string(),
    }));

    // If obj\ had been walked, this file would have a cached tree to drop.
    let resp = h.send(serde_json::json!({
        "cmd": "invalidate",
        "paths": [generated.display().to_string()],
    }));
    assert_eq!(
        resp["invalidated"].as_i64(),
        Some(0),
        "obj\\ must not be part of the project compilation: {resp}"
    );

    let real = h.send(serde_json::json!({
        "cmd": "invalidate",
        "paths": [tmp.path().join("real.vb").display().to_string()],
    }));
    assert_eq!(
        real["invalidated"].as_i64(),
        Some(1),
        "real source must still be cached: {real}"
    );
}

/// `begin_project` must parse the file list the INDEXER supplies, not
/// re-discover files by walking the tree.
///
/// The independent walk is not equivalent: the indexer applies ignore rules
/// and extension presets, the walk does not. Live consequence
/// (MiniLangCompiler, engram.log 2026-08-04 → 2026-08-20): the repo holds
/// 73,072 `.vb` files, 56,432 of them under `.tmp/` scratch trees, against
/// 917 real sources in `src/`. The sidecar parsed all of them into one
/// compilation, then timed out (188 "parse response timed out after 60s")
/// and exited (12 crashes, exit codes 0xc0000…), respawned, walked again.
#[test]
fn begin_project_uses_the_supplied_file_list() {
    let Some(bin) = sidecar_path() else {
        eprintln!("sidecar not published — skipping");
        return;
    };
    let tmp = tempfile::TempDir::new().unwrap();
    let real = tmp.path().join("real.vb");
    std::fs::write(&real, SOURCE_V1).unwrap();

    // A scratch tree the indexer would never hand over, in a directory the
    // fallback walk does not know to skip.
    let scratch_dir = tmp.path().join(".tmp").join("codex-suite");
    std::fs::create_dir_all(&scratch_dir).unwrap();
    let scratch = scratch_dir.join("generated.vb");
    std::fs::write(&scratch, SOURCE_V2).unwrap();

    let mut h = Harness::start(&bin);
    let begun = h.send(serde_json::json!({
        "cmd": "begin_project",
        "project_root": tmp.path().display().to_string(),
        "files": [real.display().to_string()],
    }));
    assert!(begun.get("error").is_none(), "begin_project: {begun}");

    // The supplied file is cached...
    let real_drop = h.send(serde_json::json!({
        "cmd": "invalidate",
        "paths": [real.display().to_string()],
    }));
    assert_eq!(
        real_drop["invalidated"].as_i64(),
        Some(1),
        "the supplied file must be in the compilation: {real_drop}"
    );

    // ...and nothing else was pulled in behind the indexer's back.
    let scratch_drop = h.send(serde_json::json!({
        "cmd": "invalidate",
        "paths": [scratch.display().to_string()],
    }));
    assert_eq!(
        scratch_drop["invalidated"].as_i64(),
        Some(0),
        "a file the indexer did not supply must not be parsed: {scratch_drop}"
    );
}

/// With no list supplied, the walk still runs — an older caller must keep
/// working — and it skips scratch trees the indexer also ignores.
#[test]
fn begin_project_without_a_list_still_walks_and_skips_scratch_trees() {
    let Some(bin) = sidecar_path() else {
        eprintln!("sidecar not published — skipping");
        return;
    };
    let tmp = tempfile::TempDir::new().unwrap();
    let real = tmp.path().join("real.vb");
    std::fs::write(&real, SOURCE_V1).unwrap();
    let scratch_dir = tmp.path().join(".tmp");
    std::fs::create_dir_all(&scratch_dir).unwrap();
    let scratch = scratch_dir.join("generated.vb");
    std::fs::write(&scratch, SOURCE_V2).unwrap();

    let mut h = Harness::start(&bin);
    h.send(serde_json::json!({
        "cmd": "begin_project",
        "project_root": tmp.path().display().to_string(),
    }));

    assert_eq!(
        h.send(serde_json::json!({
            "cmd": "invalidate",
            "paths": [real.display().to_string()],
        }))["invalidated"]
            .as_i64(),
        Some(1),
        "the fallback walk must still find real sources"
    );
    assert_eq!(
        h.send(serde_json::json!({
            "cmd": "invalidate",
            "paths": [scratch.display().to_string()],
        }))["invalidated"]
            .as_i64(),
        Some(0),
        ".tmp is a scratch tree — the walk must skip it"
    );
}

#[test]
fn constructors_have_distinct_spans_owners_arity_and_body_calls() {
    let Some(bin) = sidecar_path() else {
        eprintln!("sidecar not published — skipping");
        return;
    };
    let mut h = Harness::start(&bin);
    let source = "Namespace N\nClass Rights\nPublic Sub New()\nEnd Sub\nPublic Sub New(value As Integer)\nValidate(value)\nEnd Sub\nPrivate Sub Validate(value As Integer)\nEnd Sub\nClass Nested\nShared Sub New()\nEnd Sub\nEnd Class\nEnd Class\nEnd Namespace\n";
    let result = h.send(serde_json::json!({"cmd":"parse", "path":"rights.vb", "source":source}));
    let symbols = result["symbols"].as_array().unwrap();
    let ctors: Vec<_> = symbols
        .iter()
        .filter(|s| s["metadata"]["constructor"] == "true")
        .collect();
    assert_eq!(ctors.len(), 3, "{result}");
    assert_eq!(ctors[0]["name"], "N.Rights.New");
    assert_eq!(ctors[0]["start_line"], 3);
    assert_eq!(ctors[0]["end_line"], 4);
    assert_eq!(ctors[0]["metadata"]["arity"], "0");
    assert_eq!(ctors[1]["start_line"], 5);
    assert_eq!(ctors[1]["end_line"], 7);
    assert_eq!(ctors[1]["metadata"]["arity"], "1");
    assert_eq!(ctors[2]["name"], "N.Rights.Nested.New");
    let edges = result["edges"].as_array().unwrap();
    assert!(
        edges.iter().any(|e| e["kind"] == "calls"
            && e["source_name"] == "N.Rights.New"
            && e["source_start_line"] == 5
            && e["metadata"]["call_site_line"] == "6"),
        "{result}"
    );
    assert_eq!(
        edges
            .iter()
            .filter(|e| e["kind"] == "contains" && e["target_name"] == "N.Rights.New")
            .count(),
        2
    );
}

#[test]
fn object_creation_and_delegate_references_record_syntax_locations() {
    let Some(bin) = sidecar_path() else {
        eprintln!("sidecar not published — skipping");
        return;
    };
    let mut h = Harness::start(&bin);
    let source = "Namespace Example\nClass Widget\nPublic Sub New()\nEnd Sub\nPublic Sub New(value As Integer)\nEnd Sub\nPublic Shared Sub Factory()\nDim a = New Widget()\nDim b = New Widget(1)\nDim callback = AddressOf Factory\nEnd Sub\nEnd Class\nEnd Namespace\n";
    let result = h.send(serde_json::json!({"cmd":"parse", "path":"widget.vb", "source":source}));
    let edges = result["edges"].as_array().unwrap();
    let creations: Vec<_> = edges
        .iter()
        .filter(|e| e["metadata"]["via"] == "object_creation")
        .collect();
    assert_eq!(creations.len(), 2, "{result}");
    for (i, edge) in creations.iter().enumerate() {
        assert_eq!(edge["source_name"], "Example.Widget.Factory");
        assert_eq!(edge["target_name"], "Example.Widget.New");
        assert_eq!(edge["metadata"]["args"], i.to_string());
        assert_eq!(edge["metadata"]["call_site_line"], (8 + i).to_string());
    }
    assert!(edges.iter().any(|e| e["metadata"]["via"] == "addressof" && e["metadata"]["call_site_line"] == "10"), "{result}");
}

#[test]
fn optional_and_paramarray_parameters_record_supported_argument_bounds() {
    let Some(bin) = sidecar_path() else {
        eprintln!("sidecar not published — skipping");
        return;
    };
    let mut h = Harness::start(&bin);
    let source = "Class Sample\nPublic Sub New(required As Integer, Optional label As String = \"\")\nEnd Sub\nPublic Sub Collect(prefix As String, ParamArray values() As Integer)\nEnd Sub\nEnd Class\n";
    let result = h.send(serde_json::json!({"cmd":"parse", "path":"sample.vb", "source":source}));
    let symbols = result["symbols"].as_array().unwrap();
    let ctor = symbols.iter().find(|s| s["name"] == "Sample.New").unwrap();
    assert_eq!(ctor["metadata"]["arity"], "2");
    assert_eq!(ctor["metadata"]["arity_min"], "1");
    assert_eq!(ctor["metadata"]["arity_variadic"], "false");
    let variadic = symbols
        .iter()
        .find(|s| s["name"] == "Sample.Collect")
        .unwrap();
    assert_eq!(variadic["metadata"]["arity_min"], "1");
    assert_eq!(variadic["metadata"]["arity_variadic"], "true");
}

#[test]
fn unknown_generic_constructor_keeps_type_identity_without_parameter_syntax() {
    let Some(bin) = sidecar_path() else {
        eprintln!("sidecar not published — skipping");
        return;
    };
    let mut h = Harness::start(&bin);
    let source = "Class Factory\nSub Create()\nDim item = New External.Box(Of Integer)()\nEnd Sub\nEnd Class\n";
    let result = h.send(serde_json::json!({"cmd":"parse", "path":"factory.vb", "source":source}));
    let edges = result["edges"].as_array().unwrap();
    let edge = edges
        .iter()
        .find(|e| e["metadata"]["via"] == "object_creation")
        .unwrap();
    assert_eq!(edge["target_name"], "External.Box.New", "{result}");
}
