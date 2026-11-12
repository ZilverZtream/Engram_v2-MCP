use super::*;
use crate::parsing::SymbolExtractor;
use std::path::Path;

fn contracts(ext: &str, source: &str) -> Vec<ExtractedEdge> {
    SymbolExtractor::new()
        .extract(Path::new(&format!("contract.{ext}")), source)
        .1
        .into_iter()
        .filter(|e| matches!(e.kind.as_str(), "returns_type" | "parameter_type"))
        .collect()
}

fn uses(edge: &ExtractedEdge) -> Vec<serde_json::Value> {
    serde_json::from_str(&edge.metadata.as_ref().unwrap()["type_uses"]).unwrap()
}

#[test]
fn authored_boundaries_across_typed_languages() {
    // Independently authored signatures answer the same migration question:
    // Request flows in, Response flows out. The function body does not matter.
    for (ext, source) in [
        (
            "cs",
            "class Service { public Response Run(Request request) { return null; } }",
        ),
        ("rs", "fn run(request: Request) -> Response { todo!() }"),
        ("py", "def run(request: Request) -> Response:\n    pass\n"),
        (
            "go",
            "package service\nfunc Run(request Request) Response { panic(0) }",
        ),
        (
            "java",
            "class Service { Response run(Request request) { return null; } }",
        ),
        (
            "ts",
            "function run(request: Request): Response { throw 0; }",
        ),
        (
            "tsx",
            "const run = (request: Request): Response => { throw 0; };",
        ),
        (
            "c",
            "Response run(Request request) { return (Response){0}; }",
        ),
        ("cpp", "Response run(Request request) { return {}; }"),
    ] {
        let edges = contracts(ext, source);
        let incoming = edges
            .iter()
            .find(|e| e.kind == "parameter_type" && e.target_name == "Request")
            .unwrap_or_else(|| panic!("missing Request in {ext}: {edges:?}"));
        assert!(
            edges
                .iter()
                .any(|e| e.kind == "returns_type" && e.target_name == "Response"),
            "missing Response in {ext}: {edges:?}"
        );
        assert_eq!(uses(incoming)[0]["parameter_name"], "request", "{ext}");
        assert_eq!(uses(incoming)[0]["parameter_index"], 0, "{ext}");
        assert!(edges.iter().all(|e| e.source_start_line > 0));
    }
}

#[test]
fn generic_components_nullable_arrays_and_repeated_parameter_positions_survive() {
    let edges = contracts(
        "cs",
        "class Service { List<Response?> Run(Request[] first, Request second) { return null; } }",
    );
    let request = edges.iter().find(|e| e.target_name == "Request").unwrap();
    let occurrences = uses(request);
    assert_eq!(occurrences.len(), 2);
    assert_eq!(occurrences[0]["type_expression"], "Request[]");
    assert_eq!(occurrences[0]["parameter_name"], "first");
    assert_eq!(occurrences[1]["parameter_index"], 1);
    assert_eq!(occurrences[1]["parameter_name"], "second");
    for name in ["List", "Response"] {
        let edge = edges
            .iter()
            .find(|e| e.kind == "returns_type" && e.target_name == name)
            .unwrap();
        assert_eq!(uses(edge)[0]["type_expression"], "List<Response?>");
    }
}

#[test]
fn untyped_defaults_and_function_bodies_do_not_invent_type_facts() {
    assert!(
        contracts(
            "js",
            "function run(request = new Request()) { return new Response(); }"
        )
        .is_empty()
    );
    assert!(
        contracts(
            "py",
            "def run(request = Request()):\n    return Response()\n"
        )
        .is_empty()
    );
    let edges = contracts(
        "ts",
        "function run(request: Input = new DefaultValue()): Output { const body: Body = null; throw 0; }",
    );
    assert!(
        !edges
            .iter()
            .any(|e| matches!(e.target_name.as_str(), "DefaultValue" | "Body" | "request"))
    );
}

#[test]
fn type_parameters_are_not_mistaken_for_same_named_project_types() {
    let edges = contracts(
        "cs",
        "class T {} class Service { T Run<T>(T value) { return value; } }",
    );
    assert_eq!(edges.len(), 2);
    assert!(
        edges
            .iter()
            .all(|e| e.metadata.as_ref().unwrap()["type_reference_kind"] == "type_parameter")
    );
}

#[test]
fn qualified_references_and_overload_owners_are_preserved() {
    let edges = contracts(
        "cs",
        "class Service {\n A.Result Run(A.Request input) { return null; }\n B.Result Run(B.Request input) { return null; }\n}",
    );
    let request_a = edges.iter().find(|e| e.target_name == "A.Request").unwrap();
    let request_b = edges.iter().find(|e| e.target_name == "B.Request").unwrap();
    assert_ne!(request_a.source_start_line, request_b.source_start_line);
    assert!(
        !edges
            .iter()
            .any(|e| matches!(e.target_name.as_str(), "A" | "B" | "Request"))
    );
}

#[test]
fn go_grouped_parameters_and_multiple_results_preserve_positions() {
    let edges = contracts(
        "go",
        "package service\nfunc Run(first, second Request, third Other) (response Response, failure Error) { panic(0) }",
    );
    let request = edges.iter().find(|e| e.target_name == "Request").unwrap();
    assert_eq!(uses(request).len(), 2);
    assert_eq!(uses(request)[1]["parameter_name"], "second");
    assert_eq!(
        uses(edges.iter().find(|e| e.target_name == "Other").unwrap())[0]["parameter_index"],
        2
    );
    assert!(
        edges
            .iter()
            .any(|e| e.kind == "returns_type" && e.target_name == "Response")
    );
    assert!(
        edges
            .iter()
            .any(|e| e.kind == "returns_type" && e.target_name == "Error")
    );
}

#[test]
fn c_pointer_and_java_varargs_keep_type_modifiers() {
    let edges = contracts("c", "Response *run(Request *request) { return 0; }");
    assert_eq!(
        uses(edges.iter().find(|e| e.kind == "returns_type").unwrap())[0]["type_expression"],
        "Response *"
    );
    assert_eq!(
        uses(edges.iter().find(|e| e.kind == "parameter_type").unwrap())[0]["type_expression"],
        "Request*"
    );
    let edges = contracts(
        "java",
        "class Service { Response run(Request... requests) { return null; } }",
    );
    let incoming = edges.iter().find(|e| e.kind == "parameter_type").unwrap();
    assert_eq!(uses(incoming)[0]["type_expression"], "Request...");
    assert_eq!(uses(incoming)[0]["parameter_name"], "requests");
}

#[test]
fn tuple_labels_are_not_types_and_params_arrays_keep_their_position() {
    let edges = contracts(
        "cs",
        "class Service { (Response result, Error failure) Run(Request first, params Request[] rest) { throw null; } }",
    );
    assert!(
        !edges
            .iter()
            .any(|e| matches!(e.target_name.as_str(), "result" | "failure"))
    );
    assert!(
        edges
            .iter()
            .any(|e| e.kind == "returns_type" && e.target_name == "Response")
    );
    assert!(
        edges
            .iter()
            .any(|e| e.kind == "returns_type" && e.target_name == "Error")
    );
    let request = edges
        .iter()
        .find(|e| e.kind == "parameter_type" && e.target_name == "Request")
        .unwrap();
    let uses = uses(request);
    assert_eq!(uses.len(), 2);
    assert_eq!(uses[1]["parameter_name"], "rest");
    assert_eq!(uses[1]["parameter_index"], 1);
    assert_eq!(uses[1]["type_expression"], "Request[]");
}

#[test]
fn declarations_without_bodies_and_constructor_inputs_are_contracts_too() {
    for (ext, source) in [
        (
            "rs",
            "trait Service { fn run(request: Request) -> Response; }",
        ),
        ("c", "Response *run(Request *request);"),
        ("cpp", "class Service { Response run(Request request); };"),
        (
            "ts",
            "interface Service { run(request: Request): Response; }",
        ),
    ] {
        let edges = contracts(ext, source);
        assert!(
            edges
                .iter()
                .any(|e| e.kind == "parameter_type" && e.target_name == "Request"),
            "{ext}: {edges:?}"
        );
        assert!(
            edges
                .iter()
                .any(|e| e.kind == "returns_type" && e.target_name == "Response"),
            "{ext}: {edges:?}"
        );
    }
    for (ext, source) in [
        ("cs", "class Service { public Service(Request request) {} }"),
        (
            "java",
            "class Service { public Service(Request request) {} }",
        ),
    ] {
        let edges = contracts(ext, source);
        assert_eq!(edges.len(), 1, "{ext}: {edges:?}");
        assert_eq!(edges[0].kind, "parameter_type");
        assert_eq!(edges[0].target_name, "Request");
    }
}

#[test]
fn template_and_grouped_type_parameters_remain_unbound() {
    for (ext, source) in [
        (
            "cpp",
            "class T {}; template<typename T> T run(T request) { return request; }",
        ),
        (
            "go",
            "package sample\nfunc Run[A, B any](first A, second B) B { return second }",
        ),
        ("py", "def run[T](value: T) -> T:\n    return value\n"),
    ] {
        let edges = contracts(ext, source);
        assert!(!edges.is_empty(), "{ext}");
        for edge in edges {
            assert_eq!(
                edge.metadata.as_ref().unwrap()["type_reference_kind"],
                "type_parameter",
                "{ext}: {edge:?}"
            );
        }
    }
}

#[test]
fn array_lengths_are_preserved_as_expressions_not_types() {
    for (ext, source, expression) in [
        (
            "rs",
            "fn run<const N: usize>(request: [Request; N]) -> Response { todo!() }",
            "[Request; N]",
        ),
        (
            "go",
            "package sample\nconst N = 4\nfunc Run(request [N]Request) Response { panic(0) }",
            "[N]Request",
        ),
    ] {
        let edges = contracts(ext, source);
        assert!(
            !edges.iter().any(|e| e.target_name == "N"),
            "{ext}: {edges:?}"
        );
        let request = edges
            .iter()
            .find(|e| e.kind == "parameter_type" && e.target_name == "Request")
            .unwrap();
        assert_eq!(uses(request)[0]["type_expression"], expression);
    }
}

#[test]
fn qualified_generic_names_cannot_borrow_an_unrelated_bare_type() {
    for (ext, source) in [
        (
            "cs",
            "class Service { Contracts.Box<Request> Run(Contracts.Box<Request> value) { return null; } }",
        ),
        (
            "cpp",
            "contracts::Box<Request> run(contracts::Box<Request> value) { return {}; }",
        ),
    ] {
        let edges = contracts(ext, source);
        assert!(
            !edges.iter().any(|e| e.target_name == "Box"),
            "{ext}: {edges:?}"
        );
        for role in ["returns_type", "parameter_type"] {
            assert!(
                edges
                    .iter()
                    .any(|e| e.kind == role && e.target_name == "Request"),
                "{ext}: {edges:?}"
            );
            assert!(
                edges.iter().any(
                    |e| e.kind == role && e.target_name.to_lowercase().starts_with("contracts")
                ),
                "{ext}: {edges:?}"
            );
        }
    }
}
