//! Authored access sites, not counts inferred from the extractor's output.
use crate::parsing::{ExtractedEdge, SymbolExtractor};
use std::{collections::BTreeSet, path::Path};

#[test]
fn browser_storage_tracks_literal_preferences_and_separates_session_state() {
    let source = "function readPreference() { return localStorage.getItem('theme'); }\nfunction savePreference() { window.localStorage.setItem('theme', 'dark'); }\nfunction resetPreference() { localStorage.removeItem('theme'); }\nfunction readSession() { return sessionStorage.getItem('theme'); }\nfunction saveSession() { globalThis.sessionStorage.setItem('theme', 'light'); }\n";
    for path in ["preferences.js", "preferences.ts"] {
        let result = edges(path, source);
        assert_eq!(observed(&result, "state:LocalStorage:"), expected(&[
            ("state:LocalStorage:theme", 1, "reads_state"),
            ("state:LocalStorage:theme", 2, "writes_state"),
            ("state:LocalStorage:theme", 3, "writes_state"),
        ]));
        assert_eq!(observed(&result, "state:SessionStorage:"), expected(&[
            ("state:SessionStorage:theme", 4, "reads_state"),
            ("state:SessionStorage:theme", 5, "writes_state"),
        ]));
    }
}

fn edges(path: &str, source: &str) -> Vec<ExtractedEdge> {
    SymbolExtractor::new().extract(Path::new(path), source).1
}
fn observed(edges: &[ExtractedEdge], prefix: &str) -> BTreeSet<(String, u32, String)> {
    edges
        .iter()
        .filter(|edge| edge.target_name.starts_with(prefix))
        .map(|edge| {
            (
                edge.target_name.clone(),
                edge.source_start_line,
                edge.kind.clone(),
            )
        })
        .collect()
}
fn expected(rows: &[(&str, u32, &str)]) -> BTreeSet<(String, u32, String)> {
    rows.iter()
        .map(|(key, line, kind)| ((*key).into(), *line, (*kind).into()))
        .collect()
}

#[test]
fn authored_environment_reads_and_writes_across_four_languages() {
    for (path, source, wanted) in [
        (
            "app.ts",
            "export function readMode() { return process.env.ORDER_MODE; }\nexport function setMode() { process.env.ORDER_MODE = 'owned'; }\nexport function appendMode() { process.env.ORDER_MODE += 'x'; }\n",
            vec![
                ("state:Environment:ORDER_MODE", 1, "reads_state"),
                ("state:Environment:ORDER_MODE", 2, "writes_state"),
                ("state:Environment:ORDER_MODE", 3, "reads_state"),
                ("state:Environment:ORDER_MODE", 3, "writes_state"),
            ],
        ),
        (
            "app.py",
            "import os as system\ndef read_mode():\n    return system.getenv('ORDER_MODE')\ndef set_mode():\n    system.environ['ORDER_MODE'] = 'owned'\ndef read_again():\n    return system.environ['ORDER_MODE']\n",
            vec![
                ("state:Environment:ORDER_MODE", 3, "reads_state"),
                ("state:Environment:ORDER_MODE", 5, "writes_state"),
                ("state:Environment:ORDER_MODE", 7, "reads_state"),
            ],
        ),
        (
            "lib.rs",
            "use std::env::{var as read_var, set_var};\nfn read_mode() { let _ = read_var(\"ORDER_MODE\"); }\nfn set_mode() { unsafe { set_var(\"ORDER_MODE\", \"owned\"); } }\n",
            vec![
                ("state:Environment:ORDER_MODE", 2, "reads_state"),
                ("state:Environment:ORDER_MODE", 3, "writes_state"),
            ],
        ),
        (
            "app.go",
            "package owned\nimport system \"os\"\nfunc ReadMode() string { return system.Getenv(\"ORDER_MODE\") }\nfunc SetMode() { system.Setenv(\"ORDER_MODE\", \"owned\") }\n",
            vec![
                ("state:Environment:ORDER_MODE", 3, "reads_state"),
                ("state:Environment:ORDER_MODE", 4, "writes_state"),
            ],
        ),
    ] {
        let result = edges(path, source);
        assert_eq!(
            observed(&result, "state:Environment:"),
            expected(&wanted),
            "{path}: {result:?}"
        );
        for edge in result
            .iter()
            .filter(|edge| edge.target_name.starts_with("state:Environment:"))
        {
            assert_eq!(edge.source_kind, "function", "{path}: {edge:?}");
            let metadata = edge.metadata.as_ref().unwrap();
            let start = metadata["state_access_byte"].parse::<usize>().unwrap();
            let end = metadata["state_access_end_byte"].parse::<usize>().unwrap();
            assert!(source[start..end].contains("ORDER_MODE"));
        }
    }
}

#[test]
fn comments_strings_shadowing_and_unrelated_apis_do_not_become_environment_accesses() {
    for (path, source) in [
        (
            "app.ts",
            "// process.env.FAKE\nconst example = 'process.env.FAKE';\nfunction local(process: any) { return process.env.FAKE; }\nfunction other() { const process = {env:{FAKE:1}}; return process.env.FAKE; }\n",
        ),
        (
            "app.py",
            "import os\n# os.getenv('FAKE')\nexample = \"os.getenv('FAKE')\"\ndef local(os):\n    return os.getenv('FAKE')\ndef other():\n    os = object()\n    return os.getenv('FAKE')\n",
        ),
        (
            "lib.rs",
            "fn local() { let var = |_: &str| 1; var(\"FAKE\"); }\n// std::env::var(\"FAKE\")\n",
        ),
        (
            "app.go",
            "package owned\nimport \"os\"\nfunc local(os interface{Getenv(string) string}) string { return os.Getenv(\"FAKE\") }\n// os.Getenv(\"FAKE\")\n",
        ),
    ] {
        let result = edges(path, source);
        assert!(
            observed(&result, "state:Environment:").is_empty(),
            "{path}: {result:?}"
        );
    }
}

#[test]
fn dynamic_environment_keys_remain_explicitly_unknown() {
    let (symbols, result) = SymbolExtractor::new().extract(
        Path::new("app.ts"),
        "function mode(key: string) { return process.env[key]; }\n",
    );
    assert!(observed(&result, "state:Environment:").is_empty());
    let metadata = symbols
        .iter()
        .find(|s| s.kind == "file")
        .unwrap()
        .metadata
        .as_ref()
        .unwrap();
    let coverage: serde_json::Value =
        serde_json::from_str(&metadata[crate::source_contract::MODERN_STATE_KEY]).unwrap();
    assert!(coverage["issues"].as_array().unwrap().iter().any(|issue| {
        issue
            .as_str()
            .unwrap()
            .contains("dynamic_or_unsupported_environment_key")
    }));
}

#[test]
fn module_writes_and_reads_keep_declaration_identity_and_ignore_locals() {
    for (path, source, wanted) in [
        (
            "app.ts",
            "let total = 0;\nexport function add() { total += 1; }\nexport function read() { return total; }\nexport function local(total: number) { return total; }\n",
            vec![
                ("state:Module:app.ts::total", 1, "writes_state"),
                ("state:Module:app.ts::total", 2, "reads_state"),
                ("state:Module:app.ts::total", 2, "writes_state"),
                ("state:Module:app.ts::total", 3, "reads_state"),
            ],
        ),
        (
            "app.py",
            "total = 0\ndef add():\n    global total\n    total += 1\ndef read():\n    return total\ndef local(total):\n    return total\n",
            vec![
                ("state:Module:app.py::total", 1, "writes_state"),
                ("state:Module:app.py::total", 4, "reads_state"),
                ("state:Module:app.py::total", 4, "writes_state"),
                ("state:Module:app.py::total", 6, "reads_state"),
            ],
        ),
        (
            "lib.rs",
            "static mut TOTAL: i32 = 0;\nfn add() { unsafe { TOTAL += 1; } }\nfn read() -> i32 { unsafe { TOTAL } }\nfn local(TOTAL: i32) -> i32 { TOTAL }\n",
            vec![
                ("state:Module:lib.rs::TOTAL", 1, "writes_state"),
                ("state:Module:lib.rs::TOTAL", 2, "reads_state"),
                ("state:Module:lib.rs::TOTAL", 2, "writes_state"),
                ("state:Module:lib.rs::TOTAL", 3, "reads_state"),
            ],
        ),
        (
            "app.go",
            "package owned\nvar total int = 0\nfunc Add() { total++ }\nfunc Read() int { return total }\nfunc Local(total int) int { return total }\n",
            vec![
                ("state:Module:app.go::total", 2, "writes_state"),
                ("state:Module:app.go::total", 3, "reads_state"),
                ("state:Module:app.go::total", 3, "writes_state"),
                ("state:Module:app.go::total", 4, "reads_state"),
            ],
        ),
    ] {
        let result = edges(path, source);
        assert_eq!(
            observed(&result, "state:Module:"),
            expected(&wanted),
            "{path}: {result:?}"
        );
    }
    let same = edges("other.ts", "let total=0;\nfunction read(){return total;}\n");
    assert!(
        same.iter()
            .filter(|edge| edge.target_name.starts_with("state:Module:"))
            .all(|edge| edge.target_name == "state:Module:other.ts::total")
    );
}

#[test]
fn singleton_property_and_string_index_are_distinct_state_keys() {
    let source = "export const cache = { current: 0 };\nexport function remember(value: number) { cache.current = value; }\nexport function read() { return cache.current; }\nexport function dotted() { return cache['a.b']; }\n";
    let result = edges("cache.ts", source);
    assert_eq!(
        observed(&result, "state:Singleton:"),
        expected(&[
            ("state:Singleton:cache.ts::cache", 1, "writes_state"),
            ("state:Singleton:cache.ts::cache.current", 2, "writes_state"),
            ("state:Singleton:cache.ts::cache.current", 3, "reads_state"),
            ("state:Singleton:cache.ts::cache[\"a.b\"]", 4, "reads_state")
        ])
    );
}

#[test]
fn existing_modern_files_need_new_extraction_but_other_languages_keep_their_contract() {
    use crate::source_contract::{
        MODERN_STATE_KEY, SCRIPT_CALL_CONTRACT, SourceExtractionExpectation,
        SourceExtractionIdentity,
    };
    let expectation = SourceExtractionExpectation::for_paths(2000, ["app.ts"]).unwrap();
    for (path, source) in [
        (
            "app.ts",
            "export function read() { return process.env.MODE; }",
        ),
        ("app.py", "import os\nvalue = os.getenv('MODE')"),
        ("lib.rs", "fn read() { std::env::var(\"MODE\"); }"),
        (
            "app.go",
            "package owned\nimport \"os\"\nfunc Read() { os.Getenv(\"MODE\") }",
        ),
    ] {
        let (symbols, _) = SymbolExtractor::new().extract(Path::new(path), source);
        let mut metadata = serde_json::json!({"source_index_version":crate::SOURCE_INDEX_VERSION,
            "source_extraction_identity":SourceExtractionIdentity::for_chunk_target(2000),"script_call_contract":SCRIPT_CALL_CONTRACT});
        for (key, value) in symbols
            .iter()
            .find(|s| s.kind == "file")
            .unwrap()
            .metadata
            .as_ref()
            .unwrap()
        {
            metadata[key] = serde_json::json!(value);
        }
        assert!(
            expectation.metadata_is_compatible(&metadata, path),
            "{path}: {metadata}"
        );
        metadata.as_object_mut().unwrap().remove(MODERN_STATE_KEY);
        assert!(
            !expectation.metadata_is_compatible(&metadata, path),
            "An old unchanged file needs its new state facts"
        );
    }
    let unchanged = serde_json::json!({"source_index_version":crate::SOURCE_INDEX_VERSION,"source_extraction_identity":SourceExtractionIdentity::for_chunk_target(2000)});
    assert!(expectation.metadata_is_compatible(&unchanged, "legacy.cs"));
}

#[test]
fn react_context_creation_provider_and_consumer_use_import_bound_identity() {
    let source = "import {createContext as create, useContext as consume} from 'react';\nexport const Theme = create('light');\nexport function Provider(children: any) { return <Theme.Provider value='dark'>{children}</Theme.Provider>; }\nexport function readTheme() { return consume(Theme); }\nexport function local(Theme: any) { return consume(Theme); }\nexport function fake(consume: any) { return consume(Theme); }\n";
    let result = edges("theme.tsx", source);
    assert_eq!(
        observed(&result, "state:ReactContext:"),
        expected(&[
            ("state:ReactContext:theme.tsx::Theme", 2, "writes_state"),
            ("state:ReactContext:theme.tsx::Theme", 3, "writes_state"),
            ("state:ReactContext:theme.tsx::Theme", 4, "reads_state"),
        ]),
        "{result:?}"
    );
    let unrelated = edges(
        "other.ts",
        "import {createContext,useContext} from './unrelated';\nconst Theme=createContext('light');\nfunction read(){return useContext(Theme);}\n",
    );
    assert!(observed(&unrelated, "state:ReactContext:").is_empty());
}

#[test]
fn redux_reducer_selection_and_known_dispatch_share_the_configured_store_key() {
    let source = "import {createSlice as slice, configureStore as configure} from '@reduxjs/toolkit';\nconst orders = slice({name:'orders',initialState:{total:0},reducers:{\n add(draft) { draft.total += 1; }\n}});\nexport const store = configure({reducer:{orders:orders.reducer}});\nexport function read(){return store.getState().orders.total;}\nexport function add(){store.dispatch(orders.actions.add());}\nexport function shadow(store: any){store.dispatch(orders.actions.add());return store.getState().orders.total;}\n";
    let result = edges("orders.ts", source);
    assert_eq!(
        observed(&result, "state:Redux:"),
        expected(&[
            ("state:Redux:orders.ts::store", 5, "writes_state"),
            (
                "state:Redux:orders.ts::store.orders.total",
                3,
                "reads_state"
            ),
            (
                "state:Redux:orders.ts::store.orders.total",
                3,
                "writes_state"
            ),
            (
                "state:Redux:orders.ts::store.orders.total",
                6,
                "reads_state"
            ),
            (
                "state:Redux:orders.ts::store.orders.total",
                7,
                "writes_state"
            ),
        ]),
        "{result:?}"
    );
    let unrelated = edges(
        "other.ts",
        "import {createSlice,configureStore} from './unrelated';\nconst orders=createSlice({reducers:{add(s){s.total++;}}});\nconst store=configureStore({reducer:{orders:orders.reducer}});\nfunction read(){return store.getState().orders.total;}\n",
    );
    assert!(observed(&unrelated, "state:Redux:").is_empty());
}

#[test]
fn redux_rebound_or_nested_shadowed_state_does_not_claim_a_store_write() {
    let source = "import {createSlice} from '@reduxjs/toolkit';\nconst orders=createSlice({name:'orders',initialState:{total:0},reducers:{\n rebind(state) { state={total:1}; state.total++; },\n shadow(state) { { let state={total:1}; state.total++; } }\n}});\n";
    let (symbols, result) = SymbolExtractor::new().extract(Path::new("orders.ts"), source);
    assert!(observed(&result, "state:Redux:").is_empty(), "{result:?}");
    let coverage = &symbols
        .iter()
        .find(|s| s.kind == "file")
        .unwrap()
        .metadata
        .as_ref()
        .unwrap()[crate::source_contract::MODERN_STATE_KEY];
    assert!(coverage.contains("redux_state_parameter_rebound"));
}


#[test]
fn browser_storage_keeps_unknown_keys_and_shadowed_globals_out_of_named_state() {
    let source = r#"
function readPreference() { return localStorage.getItem('theme'); }
function dynamicPreference(key) { localStorage.setItem(key, 'dark'); }
function resetAll() { sessionStorage.clear(); }
function local(localStorage) { localStorage.setItem('decoy', 'x'); }
function localWindow(window) { return window.localStorage.getItem('decoy'); }
function nested() { const sessionStorage = other; return sessionStorage.getItem('decoy'); }
// localStorage.setItem('comment', 'x');
const example = "localStorage.setItem('string', 'x')";
"#;
    let (symbols, result) = SymbolExtractor::new().extract(Path::new("preferences.ts"), source);
    assert_eq!(observed(&result, "state:LocalStorage:"), expected(&[
        ("state:LocalStorage:theme", 2, "reads_state"),
    ]));
    assert!(observed(&result, "state:SessionStorage:").is_empty());
    let coverage = &symbols.iter().find(|symbol| symbol.kind == "file").unwrap().metadata.as_ref().unwrap()[engram_core::modern_state::KEY];
    assert!(coverage.contains("browser_storage_dynamic_key_unresolved:LocalStorage:3"));
    assert!(coverage.contains("browser_storage_clear_all_unresolved:SessionStorage:4"));
}
