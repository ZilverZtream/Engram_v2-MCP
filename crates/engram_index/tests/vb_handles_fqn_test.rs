use engram_index::vb_extractor::extract_vb;
use std::path::Path;

#[test]
fn test_vb_handles_fqn_resolution() {
    let code = r#"
Namespace MyApp.Web
    Public Class OrdersPage
        Protected Sub btnSubmit_Click(ByVal sender As Object, ByVal e As EventArgs) Handles btnSubmit.Click
        End Sub
    End Class
End Namespace
"#;
    let (_symbols, edges) = extract_vb(Path::new("Orders.aspx.vb"), code);

    let edge = edges
        .iter()
        .find(|e| e.kind == "event_wiring")
        .expect("Should find event_wiring edge");
    assert_eq!(edge.source_name, "btnSubmit");
    assert_eq!(edge.target_name, "btnSubmit_Click");

    let meta = edge.metadata.as_ref().expect("Should have metadata");
    assert_eq!(
        meta.get("fqn").unwrap(),
        "MyApp.Web.OrdersPage.btnSubmit_Click"
    );
}

#[test]
fn test_vb_handles_multi_event_resolution() {
    let code = r#"
Namespace MyApp
    Class DataPage
        Sub SaveAll() Handles btnSave.Click, btnApply.Click
        End Sub
    End Class
End Namespace
"#;
    let (_, edges) = extract_vb(Path::new("Data.aspx.vb"), code);
    let wirings: Vec<_> = edges.iter().filter(|e| e.kind == "event_wiring").collect();

    assert_eq!(wirings.len(), 2);
    for w in wirings {
        let meta = w.metadata.as_ref().unwrap();
        assert_eq!(meta.get("fqn").unwrap(), "MyApp.DataPage.SaveAll");
    }
}

#[test]
fn vb_fallback_extracts_settings_reads_and_guards() {
    let code = r#"
Namespace MyApp
    Public Class UserAdmin
        Public Sub AddUser()
            If Not Roles.IsUserInRole("Administrators") Then Return
            If Not IsContactableAdmin() Then Return
            Dim max = ConfigurationManager.AppSettings("MaxUserCount")
            Dim flag = My.Settings.AllowUserCreation
        End Sub
    End Class
End Namespace
"#;
    let (symbols, edges) = extract_vb(std::path::Path::new("UserAdmin.aspx.vb"), code);

    let setting_keys: Vec<&str> = edges
        .iter()
        .filter(|e| e.kind == "reads_setting")
        .map(|e| e.target_name.as_str())
        .collect();
    assert!(
        setting_keys.contains(&"MaxUserCount"),
        "got {setting_keys:?}"
    );
    assert!(
        setting_keys.contains(&"AllowUserCreation"),
        "My.Settings member must be detected, got {setting_keys:?}"
    );

    let add_user = symbols
        .iter()
        .find(|s| s.kind == "function" && s.name.contains("AddUser"))
        .expect("AddUser symbol");
    let meta = add_user.metadata.as_ref().expect("guard metadata");
    let checks = meta.get("permission_checks").expect("permission_checks");
    assert!(checks.contains("isuserinrole"), "got {checks}");
    assert!(
        checks.contains("iscontactableadmin"),
        "custom Is*Admin* helper must be caught, got {checks}"
    );
    assert_eq!(
        meta.get("guard_roles").map(String::as_str),
        Some("Administrators")
    );
}

#[test]
fn vb_fallback_extracts_inherits_and_implements() {
    let code = r#"
Namespace MyApp
    Public Class OrdersPage
        Inherits PageBase
        Implements IAuditable, IExportable
    End Class
End Namespace
"#;
    let (_, edges) = extract_vb(std::path::Path::new("Orders.aspx.vb"), code);
    let inherits: Vec<&str> = edges
        .iter()
        .filter(|e| e.kind == "inherits_from")
        .map(|e| e.target_name.as_str())
        .collect();
    assert_eq!(inherits, vec!["PageBase"]);
    let implements: Vec<&str> = edges
        .iter()
        .filter(|e| e.kind == "implements_interface")
        .map(|e| e.target_name.as_str())
        .collect();
    assert_eq!(implements, vec!["IAuditable", "IExportable"]);
    assert!(
        edges
            .iter()
            .filter(|e| e.kind == "inherits_from" || e.kind == "implements_interface")
            .all(|e| e.source_name == "OrdersPage"),
        "hierarchy edges must originate from the declaring class"
    );
}

#[test]
fn sidecar_enrichment_adds_settings_guards_and_hierarchy() {
    // Simulates the sidecar path: real-ranged symbols from Roslyn, then the
    // enrichment pass layered on top (exercised directly since the sidecar
    // binary is absent in tests).
    use engram_index::vb_extractor::enrich_vb_source_for_test as enrich;
    use engram_index::{ExtractedEdge, ExtractedSymbol};

    let source = r#"
Namespace MyApp
    Public Class OrdersPage
        Inherits PageBase
        Public Sub SaveOrder()
            If Not Roles.IsUserInRole("Admin") Then Return
            Dim max = ConfigurationManager.AppSettings("MaxOrders")
        End Sub
    End Class
End Namespace
"#;
    let mut symbols = vec![
        ExtractedSymbol {
            name: "OrdersPage".into(),
            kind: "class".into(),
            start_line: 3,
            end_line: 10,
            metadata: None,
        },
        ExtractedSymbol {
            name: "SaveOrder".into(),
            kind: "function".into(),
            start_line: 5,
            end_line: 8,
            metadata: None,
        },
    ];
    let mut edges: Vec<ExtractedEdge> = Vec::new();
    enrich(source, &mut symbols, &mut edges);

    assert!(
        edges
            .iter()
            .any(|e| e.kind == "inherits_from" && e.target_name == "PageBase"),
        "Inherits inside the class range must become an edge: {edges:?}"
    );
    assert!(
        edges.iter().any(|e| e.kind == "reads_setting"
            && e.target_name == "MaxOrders"
            && e.source_name == "SaveOrder"),
        "settings read must attach to the enclosing function: {edges:?}"
    );
    let meta = symbols[1].metadata.as_ref().expect("guard metadata");
    assert!(meta.get("permission_checks").unwrap().contains("isuserinrole"));
    assert_eq!(meta.get("guard_roles").map(String::as_str), Some("Admin"));
}
