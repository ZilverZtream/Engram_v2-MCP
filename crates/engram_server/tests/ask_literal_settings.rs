//! Ordinary named settings expose independently sourced stores and getter kinds.
use engram_core::{RelPath, memory::MemoryBudget};
use engram_graph::{GraphStore, Node};
use engram_server::services::ask_engine::{
    literal_settings,
    status::{AnswerStatus, ProviderStatus},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};

const BILLING: &str = "Public Class Billing\nPublic Sub Render()\nDim mode = GlobalSettings.GetText(\"InvoicePreviewMode\")\nDim other = GlobalSettings.GetInt(\"OtherMode\")\nEnd Sub\nEnd Class\n";
const TENANT: &str = "Public Class Tenant\nPublic Sub Render()\nDim flag = TenantSettings.GetBoolean(\"InvoicePreviewMode\")\nEnd Sub\nEnd Class\n";

fn fixture(root: &Path) -> (GraphStore, BTreeMap<String, u64>) {
    let graph = GraphStore::open(&root.join("graph.redb")).unwrap();
    let mut nodes = Vec::new();
    let mut selected = BTreeMap::new();
    for (path, source, store) in [
        ("Site/Billing.vb", BILLING, "globalsettings"),
        ("Site/Tenant.vb", TENANT, "tenantsettings"),
    ] {
        let target = root.join(path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, source).unwrap();
        selected.insert(path.into(), 7);
        let node = Node {
            node_id: format!("file:{path}"),
            node_type: "file".into(),
            name: path.into(),
            namespace: "memory".into(),
            language: "vbnet".into(),
            file_path: RelPath::new(path),
            start_line: 1,
            end_line: source.lines().count() as u32,
            generation: 7,
            metadata: Some(
                json!({"file_hash":blake3::hash(source.as_bytes()).to_hex().to_string()}),
            ),
        };
        let mut key = node.clone();
        key.node_id = format!("setting:{path}");
        key.node_type = "setting_key".into();
        key.name = format!("{store}[\"InvoicePreviewMode\"]");
        key.start_line = 3;
        key.end_line = 3;
        key.metadata = Some(json!({"literal_setting_key":"InvoicePreviewMode"}));
        nodes.extend([node, key]);
    }
    graph.upsert_nodes("owned", &nodes).unwrap();
    (graph, selected)
}

#[test]
fn named_setting_returns_actual_source_getters_without_conflating_independent_stores() {
    let owned = tempfile::tempdir().unwrap();
    let (graph, selected) = fixture(owned.path());
    let budget = MemoryBudget::new(128 * 1024 * 1024);
    let result = literal_settings::collect(
        &graph,
        "owned",
        owned.path(),
        &["InvoicePreviewMode".into()],
        &selected,
        &[],
        &budget,
    )
    .unwrap();
    assert_eq!(result.provider.status, ProviderStatus::Hit);
    assert_eq!(result.items.len(), 2);
    let observations: Vec<Value> = result
        .items
        .iter()
        .flat_map(|item| {
            serde_json::from_str::<Value>(&item.content).unwrap()["observations"]
                .as_array()
                .unwrap()
                .clone()
        })
        .collect();
    assert!(
        observations
            .iter()
            .any(|row| row["store"] == "globalsettings" && row["getter"] == "GetText")
    );
    assert!(
        observations
            .iter()
            .any(|row| row["store"] == "tenantsettings" && row["getter"] == "GetBoolean")
    );
    assert!(
        observations
            .iter()
            .all(|row| row["key"] == "InvoicePreviewMode" && row["line"] == 3)
    );
    for item in &result.items {
        let content: Value = serde_json::from_str(&item.content).unwrap();
        let bytes = std::fs::read(owned.path().join(item.path.as_ref().unwrap())).unwrap();
        assert_eq!(
            content["raw_blake3"],
            blake3::hash(&bytes).to_hex().to_string()
        );
        assert!(item.warnings[0].contains("physical backing-store identity"));
        assert!(item.warnings[0].contains("runtime values"));
    }
    // graph is declared after owned and drops first, before its fixture directory.
}

#[test]
fn changed_or_unselected_setting_source_is_withheld_and_scope_keeps_the_other_store() {
    let owned = tempfile::tempdir().unwrap();
    let (graph, mut selected) = fixture(owned.path());
    let budget = MemoryBudget::new(128 * 1024 * 1024);
    let keys = vec!["InvoicePreviewMode".into()];
    std::fs::write(
        owned.path().join("Site/Billing.vb"),
        format!("{BILLING}' changed\n"),
    )
    .unwrap();
    let result = literal_settings::collect(
        &graph,
        "owned",
        owned.path(),
        &keys,
        &selected,
        &[],
        &budget,
    )
    .unwrap();
    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].path.as_deref(), Some("Site/Tenant.vb"));
    assert_eq!(result.provider.status, ProviderStatus::Failed);
    assert!(matches!(
        result.qualify_status(AnswerStatus::Answered),
        AnswerStatus::Partial
    ));
    std::fs::write(owned.path().join("Site/Billing.vb"), BILLING).unwrap();
    selected.insert("Site/Billing.vb".into(), 8);
    let result = literal_settings::collect(
        &graph,
        "owned",
        owned.path(),
        &keys,
        &selected,
        &[],
        &budget,
    )
    .unwrap();
    assert_eq!(result.items.len(), 1);
    assert_eq!(result.provider.status, ProviderStatus::Failed);
    let scoped = literal_settings::collect(
        &graph,
        "owned",
        owned.path(),
        &keys,
        &selected,
        &["Site/Tenant.vb".into()],
        &budget,
    )
    .unwrap();
    assert_eq!(scoped.items.len(), 1);
    assert_eq!(scoped.provider.status, ProviderStatus::Hit);
    assert_eq!(scoped.items[0].path.as_deref(), Some("Site/Tenant.vb"));
}
