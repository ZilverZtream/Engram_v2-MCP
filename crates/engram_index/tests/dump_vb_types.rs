use tree_sitter::Parser;

#[test]
fn test_dump_node_types() {
    let lang = arborium_vb::language();
    let mut parser = Parser::new();
    parser.set_language(&lang.into()).unwrap();

    let code = r#"
Namespace App
    Class UI
        Public WithEvents btnSubmit As Button
        Public Event Submitted(id As Integer)
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
    let tree = parser.parse(code, None).unwrap();
    dump_node(&tree.root_node(), code.as_bytes(), 0);
}

fn dump_node(node: &tree_sitter::Node, source: &[u8], depth: usize) {
    println!(
        "{}{}: {}",
        "  ".repeat(depth),
        node.kind(),
        node.utf8_text(source).unwrap_or("")
    );
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        dump_node(&child, source, depth + 1);
    }
}
