use tree_sitter::Parser;

#[test]
fn test_dump_cs_node_types() {
    let lang = tree_sitter_c_sharp::LANGUAGE;
    let mut parser = Parser::new();
    parser.set_language(&lang.into()).unwrap();

    let code = r#"
namespace App {
    public partial class Foo {
        protected global::System.Web.UI.WebControls.Button btnSubmit;
    }
}"#;
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
