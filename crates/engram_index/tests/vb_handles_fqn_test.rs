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
