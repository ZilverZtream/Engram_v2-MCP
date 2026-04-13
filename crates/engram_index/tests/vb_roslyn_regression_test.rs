use engram_index::vb_extractor::extract_vb;
use std::path::Path;

#[test]
fn attribute_without_parens_method_indexed() {
    let code = r#"
Class C
  <HttpGet>
  Public Function Ping() As String
    Return "ok"
  End Function
End Class
"#;
    let (symbols, _) = extract_vb(Path::new("c.vb"), code);
    assert!(symbols.iter().any(|s| s.name.ends_with("Ping")));
}

#[test]
fn parameter_attribute_indexed() {
    let code = r#"
Class C
  Public Function Search(<FromUri> q As String) As String
    Return q
  End Function
End Class
"#;
    let (symbols, _) = extract_vb(Path::new("c.vb"), code);
    assert!(symbols.iter().any(|s| s.name.ends_with("Search")));
}

#[test]
fn implicit_continuation_signature_indexed() {
    let code = r#"
Class C
  Public Function LongMethod(
    ByVal x As Integer,
    ByVal y As Integer
  ) As Integer
    Return x + y
  End Function
End Class
"#;
    let (symbols, _) = extract_vb(Path::new("c.vb"), code);
    assert!(symbols.iter().any(|s| s.name.contains("LongMethod")));
}
