use std::path::Path;

#[test]
fn fallback_preserves_explicit_multiline_method_metadata() {
    let source = "Class Batch\n Public Shared Function CreateMany(\n id As Integer,\n Optional value As String = \"a)b\") As Boolean\n Return True\n End Function\n Private Sub Reset()\n End Sub\nEnd Class\n";
    let (symbols, _) =
        engram_index::vb_extractor::extract_vb_fallback_for_eval(Path::new("batch.vb"), source);
    let m = symbols
        .iter()
        .find(|s| s.name.ends_with("CreateMany"))
        .unwrap()
        .metadata
        .as_ref()
        .unwrap();
    assert_eq!(m.get("return_type").map(String::as_str), Some("Boolean"));
    assert_eq!(m.get("access_level").map(String::as_str), Some("Public"));
    assert!(m["signature"].contains("Optional value As String = \"a)b\") As Boolean"));
    let m = symbols
        .iter()
        .find(|s| s.name.ends_with("Reset"))
        .unwrap()
        .metadata
        .as_ref()
        .unwrap();
    assert_eq!(m["return_type"], "Void");
    assert_eq!(m["access_level"], "Private");
}
