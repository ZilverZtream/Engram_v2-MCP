use engram_index::vb_extractor::extract_vb;
use std::path::Path;

#[test]
fn vb_extraction_returns_symbols_with_roslyn_or_documented_fallback() {
    // Production prefers the Roslyn sidecar. When it is not deployed,
    // extract_vb deliberately uses the lower-fidelity fallback extractor.
    let source = r#"
Namespace Example
    Class Widget
        Sub Save()
        End Sub
    End Class
End Namespace
"#;
    let (symbols, _) = extract_vb(Path::new("sample.vb"), source);
    assert!(symbols.iter().any(|symbol| symbol.name == "Widget"));
}
