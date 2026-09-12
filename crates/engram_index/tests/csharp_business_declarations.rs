use engram_index::parsing::csharp_method_declarations;

#[test]
fn compact_declarations_keep_distinct_owners_and_literal_braces() {
    let source = r#"namespace Billing { class A { public string Save() { return "}"; } } class B { public int Save() => 2; } }"#;
    let methods = csharp_method_declarations(source);
    assert_eq!(methods.len(), 2);
    assert_eq!(methods[0].owner, "Billing.A");
    assert_eq!(methods[1].owner, "Billing.B");
    assert_eq!(methods[0].body, r#"public string Save() { return "}"; }"#);
    assert_eq!(methods[1].body, "public int Save() => 2;");
    assert!(methods.iter().all(|m| m.start_line == 1));
}

#[test]
fn generic_methods_have_complete_bodies_without_comment_or_abstract_phantoms() {
    let source = "namespace Billing;\nabstract class Invoice {\n// public int Fake() { return 1; }\npublic abstract int Missing();\npublic T Echo<T>(T value)\n{ return value; }\npublic int Echo(int value) => value;\n}\n";
    let methods = csharp_method_declarations(source);
    assert_eq!(methods.len(), 2);
    assert!(methods.iter().all(|m| m.owner == "Billing.Invoice" && m.name == "Echo"));
    assert_eq!(methods[0].start_line, 5);
    assert!(methods[0].body.ends_with("{ return value; }"));
    assert_eq!(methods[1].start_line, 7);
}
