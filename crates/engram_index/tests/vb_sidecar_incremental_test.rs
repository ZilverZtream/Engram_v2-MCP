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

/// After `begin_project`, a cached tree WINS over the source the caller
/// sends. That is the whole reason the old code had to re-scan the project
/// on every update — and the reason `invalidate` is needed rather than just
/// skipping `begin_project`.
#[test]
fn cached_tree_shadows_newer_source_until_invalidated() {
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

    let stale = h.symbol_names(&file, SOURCE_V2);
    assert!(
        stale.iter().any(|n| n.ends_with("Alpha")),
        "expected the CACHED v1 tree to win; got {stale:?}"
    );
    assert!(
        !stale.iter().any(|n| n.ends_with("Beta")),
        "characterisation: without invalidate the new source is ignored; got {stale:?}"
    );

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
        "Class Gadget\n  Public Sub Gamma()\n  End Sub\nEnd Class\n",
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

    // b.vb was untouched, so its cached tree must still be there and still
    // resolve. Sending deliberately different source proves the CACHE served
    // it rather than a fresh single-file parse.
    let names = h.symbol_names(
        &b,
        "Class Gadget\n  Public Sub Delta()\n  End Sub\nEnd Class\n",
    );
    assert!(
        names.iter().any(|n| n.ends_with("Gamma")),
        "b.vb must still be served from the warm compilation; got {names:?}"
    );
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
    let bin = sidecar_path().expect("publish the VB sidecar before this regression test");
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
    let bin = sidecar_path().expect("publish the VB sidecar before this regression test");
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
    let bin = sidecar_path().expect("publish the VB sidecar before this regression test");
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
    let bin = sidecar_path().expect("publish the VB sidecar before this regression test");
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
