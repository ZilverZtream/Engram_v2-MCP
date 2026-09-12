#![allow(clippy::unwrap_used)]
use engram_index::parsing::SymbolExtractor;
use std::path::Path;

#[test]
fn repeated_member_names_preserve_owners_without_guessing_an_unqualified_call() {
    let source = "class OrdersPanel { refresh() {} }\nclass UsersPanel { refresh() {} }\nfunction launch() { refresh(); }\n";
    for extension in ["ts", "js"] {
        let (symbols, edges) = SymbolExtractor::new()
            .extract(Path::new(&format!("panels.{extension}")), source);
        let owners: Vec<_> = symbols.iter()
            .filter(|symbol| symbol.kind == "function" && symbol.name == "refresh")
            .map(|symbol| symbol.metadata.as_ref().unwrap().get("fqn").unwrap().as_str())
            .collect();
        assert_eq!(owners, ["OrdersPanel.refresh", "UsersPanel.refresh"]);
        let calls: Vec<_> = edges.iter().filter(|edge| edge.kind == "calls").collect();
        assert_eq!(calls.len(), 1, "{edges:?}");
        assert_eq!(calls[0].source_name, "launch");
        assert_eq!(calls[0].target_name, "refresh", "An ambiguous call must not be assigned to the last class");
        for (class, method) in [("OrdersPanel", "OrdersPanel.refresh"), ("UsersPanel", "UsersPanel.refresh")] {
            assert!(edges.iter().any(|edge| edge.kind == "contains" && edge.source_name == class && edge.target_name == method), "{edges:?}");
        }
    }
}
