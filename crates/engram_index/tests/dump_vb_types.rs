use engram_index::vb_extractor::extract_vb;
use std::path::Path;

#[test]
fn test_vb_extractor_smoke() {
    let code = r#"
Namespace App
    Class UI
        Public Property Label As String
            Get
                Return ""
            End Get
        End Property
        Sub Bar()
        End Sub
    End Class
End Namespace
"#;
    let (symbols, edges) = extract_vb(Path::new("UI.vb"), code);
    assert!(!symbols.is_empty());
    assert!(edges.iter().any(|e| e.kind == "contains"));
}
