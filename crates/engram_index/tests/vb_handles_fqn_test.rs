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
    assert!(setting_keys.contains(&"MaxUserCount"), "got {setting_keys:?}");
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
