use engram_index::SymbolExtractor;
use std::path::Path;

#[test]
fn test_vb_debug() {
    engram_core::setup_test_logging();
    let code = r#"
    Namespace MyOrg
        Module M1
            Sub S1()
            End Sub
        End Module
    End Namespace
    "#;
    let extractor = SymbolExtractor::new();
    println!("DEBUG: Extractor initialized");
    let (symbols, edges) = extractor.extract(Path::new("test.vb"), code);
    println!("DEBUG: Symbols found: {:#?}", symbols);
    println!("DEBUG: Edges found: {:#?}", edges);
}
