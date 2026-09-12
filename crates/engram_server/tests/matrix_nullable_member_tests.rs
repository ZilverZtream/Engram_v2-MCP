#![allow(clippy::unwrap_used)]
use engram_core::{Config, ProjectRecord, RelPath};
use engram_graph::{Edge, EdgeKind, Node};
use engram_server::{state::AppState, tools::Engram};
use serde_json::json;
const PID: &str = "nullable-matrix";
fn node(id: &str, name: &str, kind: &str, line: u32) -> Node {
    Node {
        node_id: id.into(),
        name: name.into(),
        node_type: kind.into(),
        namespace: "Reader".into(),
        language: "vbnet".into(),
        file_path: RelPath::new("Reader.vb"),
        start_line: line,
        end_line: line,
        generation: 1,
        metadata: None,
    }
}
fn edge(target: &str, kind: EdgeKind) -> Edge {
    Edge {
        source_id: "file:Reader.vb".into(),
        target_id: target.into(),
        edge_kind: kind,
        namespace: "memory".into(),
        language: "vbnet".into(),
        weight: 1,
        generation: 1,
        metadata: None,
        updated_at_ms: 0,
    }
}
async fn matrix(
    source: &str,
    verified: bool,
    resolved: bool,
    extra: &[(&str, EdgeKind)],
) -> String {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("Reader.vb"), source).unwrap();
    let (state, _) = AppState::new(Config {
        data_dir: tmp.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })
    .unwrap();
    state
        .registry
        .put_project(&ProjectRecord {
            project_id: PID.into(),
            project_name: PID.into(),
            directory: root.to_string_lossy().into_owned(),
            project_type: "general".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            reindex_required_since_ms: None,
        })
        .unwrap();
    state
        .registry
        .set_meta(PID, "active_generation", "1")
        .unwrap();
    let mut file = node("file:Reader.vb", "Reader.vb", "file", 0);
    if verified {
        file.metadata =
            Some(json!({"file_hash":blake3::hash(source.as_bytes()).to_hex().to_string()}));
    }
    state.graph.upsert_nodes(PID, &[file]).unwrap();
    if resolved {
        state
            .graph
            .upsert_nodes(
                PID,
                &[node(
                    "::_accessCache.HasValue",
                    "OptionalAccess.HasValue",
                    "property",
                    2,
                )],
            )
            .unwrap();
    }
    let mut edges = vec![edge("::_accessCache.HasValue", EdgeKind::ReadsSetting)];
    edges.extend(extra.iter().map(|(t, k)| edge(t, k.clone())));
    state.graph.upsert_edges(PID, &edges).unwrap();
    Engram::new(state)
        .handle_derive_test_matrix(
            serde_json::from_value(json!({"project_id":PID,"files":["Reader.vb"]})).unwrap(),
        )
        .await
        .unwrap()
        .content[0]
        .as_text()
        .unwrap()
        .text
        .clone()
}
const SOURCE: &str = "Public Class Reader\n Private _accessCache As Boolean?\n Public Function Allowed() As Boolean\n  If Not _accessCache.HasValue Then _accessCache = True\n  Return _accessCache.Value\n End Function\nEnd Class\n";
#[tokio::test]
async fn unrelated_nested_types_preserve_outer_field_but_other_type_uses_do_not_bind() {
    let source = SOURCE.replace(
        "End Class",
        " Private Class Group\n  Public Name As String\n End Class\nEnd Class",
    );
    for source in [
        source.clone(),
        source.replace('\n', "\r\n"),
        format!("{SOURCE}\nPublic Class Other\nEnd Class\n"),
    ] {
        let text = matrix(&source, true, false, &[]).await;
        assert!(
            text.contains("declaration Reader.vb:2")
                && text.contains("presence check at Reader.vb:4"),
            "{text}"
        );
        assert!(
            !text.contains("setting target ::_accessCache.HasValue unavailable"),
            "{text}"
        );
    }
    let nested_use = SOURCE.replace("End Class", " Private Class Group\n  Public Function Check() As Boolean\n   Return _accessCache.HasValue\n  End Function\n End Class\nEnd Class");
    let sibling_use = format!("{SOURCE}Public Class Other\n Public Function Check() As Boolean\n  Return _accessCache.HasValue\n End Function\nEnd Class\n");
    let malformed = SOURCE.replace("End Class", "End Structure");
    let inline_type = SOURCE.replace("End Class", " Private Class Group : End Class\nEnd Class");
    let structure_use = nested_use
        .replace("Private Class Group", "Private Structure Group")
        .replace(" End Class", " End Structure");
    for source in [
        nested_use,
        sibling_use,
        malformed,
        inline_type,
        structure_use,
    ] {
        let text = matrix(&source, true, false, &[]).await;
        assert!(
            !text.contains("Local nullable state observations"),
            "{text}"
        );
        assert!(
            text.contains("setting target ::_accessCache.HasValue unavailable"),
            "{text}"
        );
    }
}
#[tokio::test]
async fn verified_nullable_cache_is_state_observation_with_exact_declaration_and_use() {
    let text = matrix(
        SOURCE,
        true,
        false,
        &[("state:Session:Selection", EdgeKind::ReadsState)],
    )
    .await;
    assert!(
        text.contains("Local nullable state observations")
            && text.contains("declaration Reader.vb:2")
            && text.contains("presence check at Reader.vb:4"),
        "{text}"
    );
    assert!(
        !text.contains("setting target ::_accessCache.HasValue unavailable"),
        "{text}"
    );
    assert!(
        !text.contains("unresolved setting target [node_id: ::_accessCache.HasValue]"),
        "{text}"
    );
    assert!(text.contains("Session:Selection"), "{text}");
}
#[tokio::test]
async fn real_nullable_setting_and_qualified_chains_are_not_blanket_filtered() {
    let source=SOURCE.replace("  Return _accessCache.Value","  If ConfigSettings.OptionalAccess.HasValue Then Return True\n  If permissions.Current.OptionalAccess.HasValue Then Return True\n  Return _accessCache.Value");
    let text = matrix(
        &source,
        true,
        true,
        &[
            (
                "::ConfigSettings.OptionalAccess.HasValue",
                EdgeKind::ReadsSetting,
            ),
            (
                "::permissions.Current.OptionalAccess.HasValue",
                EdgeKind::ReadsSetting,
            ),
        ],
    )
    .await;
    assert!(
        !text.contains("Local nullable state observations"),
        "{text}"
    );
    assert!(
        text.contains("OptionalAccess.HasValue [node_id: ::_accessCache.HasValue]")
            && text.contains("::ConfigSettings.OptionalAccess.HasValue")
            && text.contains("::permissions.Current.OptionalAccess.HasValue"),
        "{text}"
    );
    assert!(text.contains("3 setting/value references"), "{text}");
}
#[tokio::test]
async fn same_name_shadow_and_unverified_source_remain_explicitly_unresolved() {
    let shadow = SOURCE.replace(
        "End Class",
        " Public Sub Other(ByVal _accessCache As Boolean?)\n End Sub\nEnd Class",
    );
    let inferred_shadow = SOURCE.replace(
        "  Return _accessCache.Value",
        "  Dim _accessCache = Nothing\n  Return _accessCache.Value",
    );
    let lambda_shadow = SOURCE.replace(
        "  Return _accessCache.Value",
        "  Dim pick = Function(_accessCache) _accessCache.HasValue\n  Return _accessCache.Value",
    );
    for (source, verified) in [
        (&shadow[..], true),
        (&inferred_shadow[..], true),
        (&lambda_shadow[..], true),
        (SOURCE, false),
    ] {
        let text = matrix(source, verified, false, &[]).await;
        assert!(
            !text.contains("Local nullable state observations"),
            "{text}"
        );
        assert!(
            text.contains("setting target ::_accessCache.HasValue unavailable")
                && text.contains("source use line unknown"),
            "{text}"
        );
        assert!(!text.contains("Reader.vb:0"), "{text}");
        assert!(text.contains("\"regex\":false"), "{text}");
        assert!(
            text.contains("grep_project({")
                && text.contains("\"path_prefix\":\"Reader.vb\"")
                && text.contains("\"pattern\":\"_accessCache.HasValue\""),
            "{text}"
        );
    }
}
#[tokio::test]
async fn comments_strings_and_qualified_receiver_do_not_prove_local_presence_use() {
    for source in [
        SOURCE.replace(
            "If Not _accessCache.HasValue Then _accessCache = True",
            "' _accessCache.HasValue",
        ),
        SOURCE.replace(
            "If Not _accessCache.HasValue Then _accessCache = True",
            "Dim text = \"_accessCache.HasValue\"",
        ),
        SOURCE.replace(
            "Not _accessCache.HasValue",
            "Not other._accessCache.HasValue",
        ),
    ] {
        let text = matrix(&source, true, false, &[]).await;
        assert!(
            !text.contains("Local nullable state observations"),
            "{text}"
        );
        assert!(
            text.contains("setting target ::_accessCache.HasValue unavailable"),
            "{text}"
        );
    }
}
#[tokio::test]
async fn local_nullable_declaration_and_cross_method_uses_stay_unknown() {
    let local = SOURCE
        .replace(" Private _accessCache As Boolean?\n", "")
        .replace(
            " Public Function Allowed() As Boolean",
            " Public Function Allowed() As Boolean\n  Dim _accessCache As Nullable(Of Boolean)",
        );
    let text = matrix(&local, true, false, &[]).await;
    assert!(
        !text.contains("Local nullable state observations"),
        "{text}"
    );
    assert!(
        text.contains("setting target ::_accessCache.HasValue unavailable"),
        "{text}"
    );
    let cross_method = "Public Class Reader\n Public Sub First()\n  Dim _accessCache As Boolean?\n End Sub\n Public Function Second() As Boolean\n  Return _accessCache.HasValue\n End Function\nEnd Class\n";
    let text = matrix(cross_method, true, false, &[]).await;
    assert!(
        !text.contains("Local nullable state observations"),
        "{text}"
    );
    assert!(
        text.contains("setting target ::_accessCache.HasValue unavailable"),
        "{text}"
    );
}
