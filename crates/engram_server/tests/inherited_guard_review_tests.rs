//! Review the authorization contract of a VB subclass without claiming that
//! inheritance makes a newly introduced handler safe.
use engram_core::{RelPath, config::Config};
use engram_graph::{Edge, EdgeKind, Node};
use engram_server::{
    services::pre_commit_review_service::{
        Gate, GateContext, Severity, gates::GuardParityGate, parse_unified_diff,
    },
    state::AppState,
};
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};

const CHILD: &str = "Public Class Derived\n Inherits SecureBase\n <WebMethod()>\n Public Sub Add()\n  Save()\n End Sub\nEnd Class\n";
const BASE: &str = "Public Class SecureBase\n Protected Sub Page_Load()\n  RequireRole(\"Admin\")\n End Sub\nEnd Class\n";

struct Fixture {
    // AppState and all database handles close before the owned directory.
    state: AppState,
    root: PathBuf,
    _owned: tempfile::TempDir,
}
impl Fixture {
    fn new(child: &str, base: &str, inherit: bool) -> Self {
        let owned = tempfile::tempdir().unwrap();
        let root = owned.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        let (state, _rx) = AppState::new(Config {
            data_dir: owned.path().join("data"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            ..Default::default()
        })
        .unwrap();
        let fixture = Self {
            state,
            root,
            _owned: owned,
        };
        fixture.add_type("Derived", "Derived.vb", child);
        fixture.add_type("SecureBase", "Base.vb", base);
        if inherit {
            fixture.inherits("Derived", "SecureBase", 1);
        }
        fixture
    }
    fn add_type(&self, name: &str, path: &str, source: &str) {
        std::fs::write(self.root.join(path), source).unwrap();
        let class = Node {
            node_id: name.into(),
            node_type: "class".into(),
            name: name.into(),
            namespace: "memory".into(),
            language: "vbnet".into(),
            file_path: RelPath::new(path),
            start_line: 1,
            end_line: source.lines().count() as u32,
            generation: 1,
            metadata: None,
        };
        let file = Node {
            node_id: format!("file:{path}"),
            node_type: "file".into(),
            name: path.into(),
            metadata: Some(
                json!({"file_hash":blake3::hash(source.as_bytes()).to_hex().to_string()}),
            ),
            ..class.clone()
        };
        self.state
            .graph
            .upsert_nodes("owned", &[class, file])
            .unwrap();
    }
    fn inherits(&self, from: &str, to: &str, generation: u64) {
        self.state
            .graph
            .upsert_edges(
                "owned",
                &[Edge {
                    source_id: from.into(),
                    target_id: to.into(),
                    edge_kind: EdgeKind::InheritsFrom,
                    namespace: "memory".into(),
                    language: "vbnet".into(),
                    weight: 1,
                    generation,
                    metadata: None,
                    updated_at_ms: 1,
                }],
            )
            .unwrap();
    }
    fn review(
        &self,
    ) -> (
        Vec<engram_server::services::pre_commit_review_service::ReviewFinding>,
        Vec<String>,
    ) {
        let diff = parse_unified_diff(
            "diff --git a/Derived.vb b/Derived.vb\n--- a/Derived.vb\n+++ b/Derived.vb\n@@ -2,0 +3,4 @@\n+ <WebMethod()>\n+ Public Sub Add()\n+  Save()\n+ End Sub\n",
        );
        let changed_paths = HashSet::from(["Derived.vb".to_owned()]);
        let ctx = GateContext {
            state: &self.state,
            graph: self.state.graph.clone(),
            registry: self.state.registry.clone(),
            project_id: "owned",
            project_dir: &self.root,
            generation: 1,
            diff_files: &diff,
            changed_paths: &changed_paths,
            total_commits: 1,
            repo_rules: Arc::new(Vec::new()),
            files_by_parent: Arc::new(HashMap::new()),
            audit_function: None,
            search_index_note: None,
            degraded: Mutex::new(Vec::new()),
            caps: Mutex::new(Vec::new()),
        };
        (GuardParityGate.run(&ctx).unwrap(), ctx.take_degraded())
    }
}

#[test]
fn inherited_convention_is_cited_for_a_new_handler_without_claiming_protection() {
    let fixture = Fixture::new(CHILD, BASE, true);
    let (findings, degraded) = fixture.review();
    assert!(degraded.is_empty(), "{degraded:?}");
    assert_eq!(findings.len(), 1);
    let finding = &findings[0];
    assert_eq!(finding.severity, Severity::Info);
    assert!(finding.detail.contains("does not establish"));
    let citation = &finding.evidence[0];
    assert!(citation.contains("requirerole at Base.vb:3 in SecureBase"));
    assert!(citation.contains(&blake3::hash(BASE.as_bytes()).to_hex().to_string()));
    assert!(
        finding
            .suggestion
            .contains("Inspect the cited base methods")
    );
}

#[test]
fn inherited_calls_do_not_suppress_a_missing_local_sibling_guard_warning() {
    let child = CHILD.replace(
        "End Class",
        " Public Sub Existing()\n  CheckAccess()\n End Sub\nEnd Class",
    );
    let fixture = Fixture::new(&child, BASE, true);
    let (findings, _) = fixture.review();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].severity, Severity::Warning);
    assert!(
        findings[0]
            .evidence
            .iter()
            .any(|item| item.contains("sibling guards") && item.contains("checkaccess"))
    );
    assert!(
        findings[0]
            .evidence
            .iter()
            .any(|item| item.contains("Base.vb:3"))
    );
}

#[test]
fn changed_base_source_withholds_old_conventions_and_discloses_missing_coverage() {
    let fixture = Fixture::new(CHILD, BASE, true);
    std::fs::write(
        fixture.root.join("Base.vb"),
        BASE.replace("RequireRole", "OldAuthorizationRemoved"),
    )
    .unwrap();
    let (findings, degraded) = fixture.review();
    assert!(findings.is_empty());
    assert!(
        degraded
            .iter()
            .any(|note| note.contains("differs from its indexed revision"))
    );
}

#[test]
fn unrelated_same_name_source_is_not_inferred_as_an_inheritance_edge() {
    let fixture = Fixture::new(CHILD, BASE, false);
    let (findings, degraded) = fixture.review();
    assert!(findings.is_empty());
    assert!(degraded.is_empty());
}

#[test]
fn adjacent_class_does_not_lend_its_base_guards_to_the_changed_handler() {
    let child = format!("{CHILD}Public Class Other\n Inherits SecureBase\nEnd Class\n");
    let fixture = Fixture::new(&child, BASE, false);
    let mut derived = fixture
        .state
        .graph
        .get_node("owned", "Derived")
        .unwrap()
        .unwrap();
    derived.end_line = CHILD.lines().count() as u32;
    let other = Node {
        node_id: "Other".into(),
        name: "Other".into(),
        start_line: 8,
        end_line: child.lines().count() as u32,
        ..derived.clone()
    };
    fixture
        .state
        .graph
        .upsert_nodes("owned", &[derived, other])
        .unwrap();
    fixture.inherits("Other", "SecureBase", 1);
    let (findings, degraded) = fixture.review();
    assert!(findings.is_empty());
    assert!(degraded.is_empty());
}

#[test]
fn base_documentation_and_string_examples_do_not_become_guard_calls() {
    let base = "Public Class SecureBase\n ' RequireRole(\"Admin\")\n Dim example As String = \"CheckAccess()\"\n Protected Sub RequireAdmin()\n End Sub\nEnd Class\n";
    let fixture = Fixture::new(CHILD, base, true);
    let (findings, degraded) = fixture.review();
    assert!(findings.is_empty());
    assert!(degraded.is_empty());
}

#[test]
fn transitive_base_conventions_remain_cited_and_future_edges_remain_untrusted() {
    let base = "Public Class SecureBase\n Inherits RootBase\nEnd Class\n";
    let fixture = Fixture::new(CHILD, base, true);
    fixture.add_type(
        "RootBase",
        "Root.vb",
        &BASE.replace("SecureBase", "RootBase"),
    );
    fixture.inherits("SecureBase", "RootBase", 1);
    let (findings, degraded) = fixture.review();
    assert_eq!(findings.len(), 1);
    assert!(
        findings[0]
            .evidence
            .iter()
            .any(|item| item.contains("Root.vb:3 in RootBase"))
    );
    assert!(degraded.is_empty());
    fixture.inherits("SecureBase", "RootBase", 2);
    let (findings, degraded) = fixture.review();
    assert!(findings.is_empty());
    assert!(
        degraded
            .iter()
            .any(|note| note.contains("differs from its selected type revision"))
    );
}
