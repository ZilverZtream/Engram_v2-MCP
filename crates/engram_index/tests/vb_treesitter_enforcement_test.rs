use engram_index::vb_extractor::extract_vb;
use std::path::Path;

#[test]
fn test_vb_treesitter_path_runs() {
    // This test ensures that the tree-sitter path is functioning.
    // By setting the env var, we force extract_vb to panic if it would otherwise fallback to regex.
    unsafe {
        std::env::set_var("ENGRAM_REQUIRE_VB_TREESITTER", "1");
    }

    let code = r#"
Namespace Test
    Class Foo
        Sub Bar()
        End Sub
    End Class
End Namespace
"#;
    let (symbols, _) = extract_vb(Path::new("test.vb"), code);

    assert!(
        !symbols.is_empty(),
        "Should extract symbols via tree-sitter"
    );
    assert_eq!(symbols[0].name, "Foo");
}
