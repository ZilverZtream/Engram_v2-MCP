use crate::parsing::{ExtractedEdge, ExtractedSymbol, SymbolExtractor};
use engram_core::typescript_modules::{CALL_KEY, Call, MODULE_KEY, Module};
use std::path::Path;

fn extract(path: &str, source: &str) -> (Module, Vec<ExtractedSymbol>, Vec<ExtractedEdge>) {
    let (symbols, edges) = SymbolExtractor::new().extract(Path::new(path), source);
    let facts = symbols
        .iter()
        .find(|s| s.kind == "file")
        .unwrap()
        .metadata
        .as_ref()
        .unwrap();
    (
        serde_json::from_str(&facts[MODULE_KEY]).unwrap(),
        symbols,
        edges,
    )
}

fn calls(edges: &[ExtractedEdge]) -> Vec<Call> {
    edges
        .iter()
        .filter_map(|e| e.metadata.as_ref()?.get(CALL_KEY))
        .map(|s| serde_json::from_str(s).unwrap())
        .collect()
}

#[test]
fn renamed_default_and_namespace_calls_keep_authored_identity_and_distinct_sites() {
    let source = "import send, { save as persist } from './left';\nimport * as right from './right';\nexport function run() { persist(); right.save(); send(); persist(); }\n";
    let (facts, _, edges) = extract("app.ts", source);
    assert!(facts.issues.is_empty(), "{:?}", facts.issues);
    let calls = calls(&edges);
    assert_eq!(calls.len(), 4);
    assert_eq!(
        calls
            .iter()
            .map(|c| (c.local.as_str(), c.imported.as_str(), c.module.as_str()))
            .collect::<Vec<_>>(),
        [
            ("persist", "save", "./left"),
            ("right", "save", "./right"),
            ("send", "default", "./left"),
            ("persist", "save", "./left")
        ]
    );
    assert!(
        calls
            .iter()
            .all(|c| c.unbound_reason.is_none() && c.site.line == 3)
    );
    for call in &calls {
        assert_eq!(
            &source[call.site.offset..call.site.end_offset],
            if call.local == "right" {
                "save"
            } else {
                &call.local
            }
        );
        assert!(call.import_site.line < call.site.line);
    }
    assert!(
        calls
            .windows(2)
            .all(|pair| pair[0].site.offset < pair[1].site.offset)
    );
    assert!(
        edges
            .iter()
            .filter(|e| e.kind == "calls")
            .all(|e| e.target_kind.is_none())
    );
}

#[test]
fn exports_preserve_local_aliases_overloads_default_and_barrel_provenance() {
    let source = "export function save(x: string): void;\nexport function save(x: string) {}\nconst write = () => 1;\nexport { write as send };\nexport default save;\nexport { save as remote } from './other';\nexport * from './star';\nexport * as ns from './namespace';\nexport type { Shape } from './types';\n";
    let (facts, _, _) = extract("barrel.ts", source);
    assert!(facts.issues.is_empty(), "{:?}", facts.issues);
    let save: Vec<_> = facts
        .declarations
        .iter()
        .filter(|d| d.local == "save")
        .collect();
    assert_eq!(save.len(), 2);
    assert!(!save[0].has_body && save[1].has_body);
    assert_eq!(save[1].site.line, 2);
    let lookup = |name: &str| facts.exports.iter().find(|e| e.exported == name).unwrap();
    assert_eq!(lookup("send").local, "write");
    assert_eq!(lookup("default").local, "save");
    assert_eq!(
        (
            lookup("remote").local.as_str(),
            lookup("remote").module.as_deref()
        ),
        ("save", Some("./other"))
    );
    assert_eq!(
        (lookup("*").kind.as_str(), lookup("*").module.as_deref()),
        ("star", Some("./star"))
    );
    assert_eq!(lookup("ns").kind, "namespace");
    assert!(lookup("Shape").type_only);
}

#[test]
fn default_callable_declarations_have_real_nodes_and_own_their_calls() {
    for source in [
        "import { save } from './storage';\nexport default function () { return save(); }",
        "import { save } from './storage';\nexport default () => save();",
        "import { save } from './storage';\nexport default function submit() { return save(); }",
    ] {
        let (facts, symbols, edges) = extract("default.ts", source);
        assert!(facts.issues.is_empty(), "{:?}", facts.issues);
        let export = facts
            .exports
            .iter()
            .find(|e| e.exported == "default")
            .unwrap();
        let declaration = facts
            .declarations
            .iter()
            .find(|d| d.local == export.local)
            .unwrap();
        assert!(declaration.callable && declaration.has_body);
        assert!(
            symbols
                .iter()
                .any(|s| s.kind == "function" && s.name == export.local && s.start_line == 2),
            "{symbols:?}"
        );
        let call = edges.iter().find(|e| e.kind == "calls").unwrap();
        assert_eq!(call.source_name, export.local);
        assert_eq!(call.source_start_line, 2);
    }
}

#[test]
fn tsx_uses_jsx_grammar_and_top_level_import_calls_have_file_owner() {
    let source = "import { save as persist } from './store';\npersist();\nexport const View = () => <button onClick={() => persist()}>Save</button>;";
    let (facts, symbols, edges) = extract("view.tsx", source);
    assert!(facts.issues.is_empty(), "{:?}", facts.issues);
    assert!(
        symbols
            .iter()
            .any(|s| s.name == "View" && s.kind == "function")
    );
    assert_eq!(calls(&edges).len(), 2);
    let call = edges
        .iter()
        .find(|e| e.kind == "calls" && e.source_kind == "file")
        .unwrap();
    assert_eq!(call.source_name, "file");
    assert_eq!(calls(&[call.clone()])[0].site.line, 2);
}

#[test]
fn parameter_destructuring_and_block_shadows_do_not_escape_their_scopes() {
    let source = "import { save } from './store';\nfunction parameter(save: () => void) { save(); }\nfunction destructured({ key: save }: any) { save(); }\nfunction propertyOnly({ save: other }: any) { save(); }\nfunction blocks() {\n { save(); const save = () => 0; }\n save();\n}\nfunction varHoist() { save(); if (true) { var save = () => 0; } }\nfunction catches() { try {} catch (save) { save(); } save(); }\nfunction loops(xs: any[]) { for (const save of xs) { save(); } save(); }\n";
    let (_, _, edges) = extract("scope.ts", source);
    let calls = calls(&edges);
    assert_eq!(
        calls
            .iter()
            .map(|c| (c.site.line, c.unbound_reason.as_deref()))
            .collect::<Vec<_>>(),
        vec![
            (2, Some("shadowed_import")),
            (3, Some("shadowed_import")),
            (4, None),
            (6, Some("shadowed_import")),
            (7, None),
            (9, Some("shadowed_import")),
            (10, Some("shadowed_import")),
            (10, None),
            (11, Some("shadowed_import")),
            (11, None),
        ]
    );
}

#[test]
fn type_only_and_object_imports_cannot_become_runtime_callable_proof() {
    let source = "import type { save } from './store';\nimport { type send, object } from './other';\nimport * as ns from './ns';\nfunction run() { save(); send(); object.save(); ns(); }";
    let (_, _, edges) = extract("type-only.ts", source);
    assert_eq!(
        calls(&edges)
            .iter()
            .map(|c| c.unbound_reason.as_deref())
            .collect::<Vec<_>>(),
        [
            Some("type_only_import"),
            Some("type_only_import"),
            Some("unsupported_import_receiver"),
            Some("namespace_not_callable"),
        ]
    );
}

#[test]
fn malformed_and_escaped_modules_do_not_supply_binding_proof() {
    let (facts, _, _) = extract(
        "escaped.ts",
        "import { save } from './st\\u006fre'; export { save };",
    );
    assert_eq!(
        facts.imports[0].local, "save",
        "Keep the binding to fence unsupported calls"
    );
    assert!(!facts.issues.is_empty());
    let (_, _, edges) = extract(
        "broken.ts",
        "import { save } from './store'; function run() { save(); const = ; }",
    );
    assert_eq!(
        calls(&edges)[0].unbound_reason.as_deref(),
        Some("incomplete_module_syntax")
    );
}

#[test]
fn receiver_variables_stay_unbound_and_import_alias_cannot_borrow_local_fqn() {
    let source = "import { save as persist } from './store';\nfunction run(items: any[]) { for (const item of items) { item.save(); } persist(); }\nclass Other { persist() {} save() {} }";
    let (_, _, edges) = extract("receivers.ts", source);
    let receiver = edges.iter().find(|e| e.target_name == "item.save").unwrap();
    assert!(receiver.target_kind.is_none());
    assert_eq!(
        receiver.metadata.as_ref().unwrap()["receiver_unbound"],
        "true"
    );
    let imported = edges.iter().find(|e| e.target_name == "persist").unwrap();
    assert!(imported.target_kind.is_none());
    assert_eq!(calls(&[imported.clone()])[0].module, "./store");
}

#[test]
fn dynamic_require_and_escaped_imports_never_fall_back_to_same_named_functions() {
    for (source, reason) in [
        (
            "import { save } from './st\\u006fre'; function run() { save(); }",
            "incomplete_module_syntax",
        ),
        (
            "import save = require('./store'); function run() { save(); }",
            "incomplete_module_syntax",
        ),
        (
            "async function run() { const { save } = await import('./store'); save(); }",
            "unsupported_dynamic_import",
        ),
        (
            "function run() { const { save } = require('./store'); save(); }",
            "unsupported_dynamic_import",
        ),
    ] {
        let (_, _, edges) = extract("unsupported.ts", source);
        let calls = calls(&edges);
        assert_eq!(calls.len(), 1, "{source}: {edges:?}");
        assert_eq!(calls[0].unbound_reason.as_deref(), Some(reason));
    }
}

#[test]
fn named_class_expression_shadows_import_only_inside_class() {
    let source = "import { save } from './store'; const C = class save { run() { save(); } }; function outside() { save(); }";
    let (_, _, edges) = extract("class-expression.ts", source);
    assert_eq!(
        calls(&edges)
            .iter()
            .map(|c| c.unbound_reason.as_deref())
            .collect::<Vec<_>>(),
        [Some("shadowed_import"), None]
    );
}
