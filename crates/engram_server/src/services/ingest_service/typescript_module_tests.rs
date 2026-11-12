//! Expected identities come from these authored files, through the real native
//! extractor, ingestion preparation, graph pass and public reference handler.
use super::*;
use engram_core::{NodeId, RelPath};
use engram_graph::EdgeKind;
use std::sync::Arc;

fn stats(files: &[(&str, &str)]) -> engram_index::IngestStats {
    let mut stats = engram_index::IngestStats::default();
    for &(path, source) in files {
        let path = Arc::new(RelPath::new(path));
        let (symbols, edges) = engram_index::parsing::SymbolExtractor::new()
            .extract(std::path::Path::new(path.as_str()), source);
        stats
            .fingerprints
            .push(engram_index::docstore::FileFingerprint {
                rel_path: path.as_str().into(),
                size: source.len() as u64,
                mtime_ms: 1,
                mtime_ns: None,
                file_hash: blake3::hash(source.as_bytes()).to_hex().to_string(),
            });
        stats.all_files.push((*path).clone());
        stats
            .symbols
            .extend(symbols.into_iter().map(|s| (path.clone(), s)));
        stats
            .edges
            .extend(edges.into_iter().map(|e| (path.clone(), e)));
    }
    stats
}

fn state(root: &std::path::Path) -> anyhow::Result<AppState> {
    let (state, _events) = AppState::new(engram_core::Config {
        data_dir: root.join("data"),
        allowed_roots: vec![root.to_path_buf()],
        embedding_backend: "fts_only".into(),
        ..Default::default()
    })?;
    Ok(state)
}

fn outgoing(state: &AppState, caller: &str) -> anyhow::Result<Vec<engram_graph::Edge>> {
    let nodes = state
        .graph
        .query_nodes("tsmodules", Some("function"), Some(caller), None, 100)?;
    let node = nodes.iter().find(|n| n.name == caller).unwrap();
    Ok(state
        .graph
        .edges_touching("tsmodules", &node.node_id, 100)?
        .into_iter()
        .filter(|e| e.edge_kind == EdgeKind::Calls && e.source_id == node.node_id)
        .collect())
}

#[tokio::test]
async fn typescript_modules_bind_import_identity_and_publish_correct_public_calls()
-> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let result = async {
        let state = state(temporary.path())?;
        let files = [
            ("lib/left.ts", "export function save() { return 11; }"),
            ("lib/right.ts", "export function save() { return 22; }"),
            ("lib/default.ts", "export default function () { return 33; }"),
            ("lib/folder/index.ts", "export const save = () => 44;"),
            ("lib/barrel.ts", "export { save as publicSave } from './left';"),
            ("decoy.ts", "export function persist() { return 99; }"),
            ("app.ts", "import { save as persist } from './lib/left';\nimport * as right from './lib/right';\nimport send from './lib/default';\nimport { save as folder } from './lib/folder';\nimport { publicSave } from './lib/barrel.js';\nexport function RunNamed() { persist(); persist(); }\nexport function RunNamespace() { return right.save(); }\nexport function RunDefault() { return send(); }\nexport function RunFolder() { return folder(); }\nexport function RunBarrel() { return publicSave(); }"),
        ];
        process_ingest_stats(&state, "tsmodules", 1, &stats(&files)).await?;
        state.graph.resolve_symbol_edges("tsmodules")?;
        let app = crate::tools::Engram::new(state.clone());
        for (caller, path, name) in [
            ("RunNamed", "lib/left.ts", "save"), ("RunNamespace", "lib/right.ts", "save"),
            ("RunDefault", "lib/default.ts", "default"), ("RunFolder", "lib/folder/index.ts", "save"),
            ("RunBarrel", "lib/left.ts", "save"),
        ] {
            let edges = outgoing(&state, caller)?;
            assert_eq!(edges.len(), 1, "{caller}: {edges:?}");
            let target = NodeId::symbol("function", None, path, name, 1).0;
            assert_eq!(edges[0].target_id, target, "{caller}");
            let metadata = edges[0].metadata.as_ref().unwrap();
            assert_eq!(metadata["resolution"], "syntax_module_export");
            let bindings = metadata["typescript_module_bindings_v1"].as_array().unwrap();
            assert_eq!(bindings.len(), if caller == "RunNamed" { 2 } else { 1 });
            assert_eq!(bindings[0]["target_file"], path);
            assert_eq!(bindings[0]["target_line"], 1);
            let reply = app.handle_find_references(serde_json::from_value(serde_json::json!({
                "project_id":"tsmodules", "node_id":edges[0].source_id, "edge_kind":"calls", "direction":"out"
            }))?).await.map_err(|e| anyhow::anyhow!("{e}"))?;
            let shown = &reply.content[0].as_text().unwrap().text;
            assert!(shown.contains(&target), "{shown}");
            assert!(!shown.contains("decoy.ts"), "{shown}");
            assert!(shown.contains("syntax_only_not_compiler_or_runtime_proof"), "{shown}");
            assert!(shown.contains("\"source_file\":\"app.ts\""), "{shown}");
            assert!(shown.contains("\"import_site\":"), "{shown}");
        }
        assert_eq!(state.graph.resolve_symbol_edges("tsmodules")?, 0, "Unchanged evidence is idempotent");
        Ok(())
    }.await;
    temporary.close()?;
    result
}

#[tokio::test]
async fn typescript_modules_retract_changed_exports_with_unchanged_importer() -> anyhow::Result<()>
{
    let temporary = tempfile::tempdir()?;
    let result = async {
        let state = state(temporary.path())?;
        let caller =
            "import { save as persist } from './api';\nexport function Run() { return persist(); }";
        process_ingest_stats(
            &state,
            "tsmodules",
            1,
            &stats(&[
                ("app.ts", caller),
                ("api.ts", "export function save() { return 1; }"),
                ("decoy.ts", "export function persist() { return 99; }"),
            ]),
        )
        .await?;
        state.graph.resolve_symbol_edges("tsmodules")?;
        assert_eq!(
            outgoing(&state, "Run")?[0].target_id,
            "sym:function:api.ts:save:1"
        );
        let caller_before = state.graph.get_node("tsmodules", "file:app.ts")?.unwrap();
        // Only target metadata changes here. Even a stale same-named target
        // declaration cannot supply an export that the source no longer has.
        process_ingest_stats(
            &state,
            "tsmodules",
            2,
            &stats(&[("api.ts", "function save() { return 2; }")]),
        )
        .await?;
        state.graph.resolve_symbol_edges("tsmodules")?;
        let edges = outgoing(&state, "Run")?;
        assert_eq!(edges.len(), 1);
        assert!(
            edges[0]
                .target_id
                .starts_with(engram_core::typescript_modules::UNBOUND_PREFIX)
        );
        assert_eq!(
            edges[0].metadata.as_ref().unwrap()["typescript_module_bindings_v1"][0]["reason"],
            "export_not_found"
        );
        process_ingest_stats(
            &state,
            "tsmodules",
            3,
            &stats(&[("api.ts", "\nexport function save() { return 3; }")]),
        )
        .await?;
        state.graph.resolve_symbol_edges("tsmodules")?;
        assert_eq!(
            outgoing(&state, "Run")?[0].target_id,
            "sym:function:api.ts:save:2"
        );
        let caller_after = state.graph.get_node("tsmodules", "file:app.ts")?.unwrap();
        assert_eq!(
            bincode::serialize(&caller_before)?,
            bincode::serialize(&caller_after)?
        );
        Ok(())
    }
    .await;
    temporary.close()?;
    result
}

#[tokio::test]
async fn typescript_modules_keep_ambiguous_cyclic_external_and_shadowed_calls_unbound()
-> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let result = async {
        let state = state(temporary.path())?;
        process_ingest_stats(&state, "tsmodules", 1, &stats(&[
            ("left.ts", "export function save() {}"), ("right.ts", "export function save() {}"),
            ("ambiguous.ts", "export * from './left'; export * from './right';"),
            ("cycle-a.ts", "export { save } from './cycle-b';"), ("cycle-b.ts", "export { save } from './cycle-a';"),
            ("app.ts", "import { save as ambiguous } from './ambiguous';\nimport { save as cycle } from './cycle-a';\nimport { save as external } from 'package';\nimport { save as shadow } from './left';\nfunction Ambiguous() { ambiguous(); }\nfunction Cyclic() { cycle(); }\nfunction External() { external(); }\nfunction Shadow(shadow: () => void) { shadow(); }"),
            ("decoy.ts", "function ambiguous() {} function cycle() {} function external() {} function shadow() {}"),
        ])).await?;
        state.graph.resolve_symbol_edges("tsmodules")?;
        for (caller, reason) in [("Ambiguous", "ambiguous_export"), ("Cyclic", "cyclic_export"), ("External", "external_or_unmapped_module"), ("Shadow", "shadowed_import")] {
            let edges = outgoing(&state, caller)?;
            assert_eq!(edges.len(), 1);
            assert!(edges[0].target_id.starts_with(engram_core::typescript_modules::UNBOUND_PREFIX), "{caller}: {edges:?}");
            assert_eq!(edges[0].metadata.as_ref().unwrap()["typescript_module_bindings_v1"][0]["reason"], reason);
            assert!(matches!(state.graph.resolve_symbol("tsmodules", &edges[0].target_id, None, None)?, engram_graph::store::ResolveResult::NotFound));
        }
        Ok(())
    }.await;
    temporary.close()?;
    result
}

#[tokio::test]
async fn typescript_modules_self_import_preserves_ordinary_local_call() -> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let result = async {
        let state = state(temporary.path())?;
        let original = "import { save as imported } from './self';\nexport function save() {}\nfunction Run() { save(); imported(); }";
        process_ingest_stats(&state, "tsmodules", 1, &stats(&[("self.ts", original)])).await?;
        state.graph.resolve_symbol_edges("tsmodules")?;
        let edges = outgoing(&state, "Run")?;
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_id, "sym:function:self.ts:save:2");
        assert_eq!(edges[0].metadata.as_ref().unwrap()["typescript_module_bindings_v1"].as_array().unwrap().len(), 1);
        assert_eq!(state.graph.resolve_symbol_edges("tsmodules")?, 0);
        // Removing an export must preserve the independently authored local call.
        process_ingest_stats(&state, "tsmodules", 2, &stats(&[("self.ts", &original.replace("export function", "function"))])).await?;
        state.graph.resolve_symbol_edges("tsmodules")?;
        let edges = outgoing(&state, "Run")?;
        assert_eq!(edges.len(), 2);
        let local = edges.iter().find(|e| e.target_id == "sym:function:self.ts:save:2").unwrap();
        assert!(local.metadata.as_ref().unwrap().get("typescript_module_bindings_v1").is_none());
        assert!(edges.iter().any(|e| e.target_id.starts_with(engram_core::typescript_modules::UNBOUND_PREFIX)));
        Ok(())
    }.await;
    temporary.close()?;
    result
}

#[tokio::test]
async fn typescript_modules_config_aliases_inherit_origin_and_rebind_unchanged_callers()
-> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let result = async {
        let state = state(temporary.path())?;
        let config = r#"{ // root config
            "compilerOptions": {"baseUrl":".", "paths": {
                "@lib/*":["missing/*","lib/*"], "exact":["lib/left"], "@lib/special/*":["lib/right"]
            },},
        }"#;
        let files = [
            ("tsconfig.json", config),
            ("config/base.json", r#"{"compilerOptions":{"paths":{"@/*":["../lib/*"]}}}"#),
            ("nested/tsconfig.json", r#"{"extends":"../config/base"}"#),
            ("lib/left.ts", "export function save() { return 1; }"),
            ("lib/right.ts", "export function save() { return 2; }"),
            ("app.ts", "import { save as mapped } from '@lib/left';\nimport { save as base } from 'lib/right';\nimport { save as exact } from 'exact';\nimport { save as specific } from '@lib/special/left';\nfunction Mapped() { mapped(); }\nfunction Base() { base(); }\nfunction Exact() { exact(); }\nfunction Specific() { specific(); }"),
            ("nested/app.ts", "import { save as inherited } from '@/left';\nfunction Inherited() { inherited(); }"),
            ("decoy.ts", "function mapped() {} function inherited() {}"),
        ];
        process_ingest_stats(&state, "tsmodules", 1, &stats(&files)).await?;
        state.graph.resolve_symbol_edges("tsmodules")?;
        for (caller, path) in [("Mapped","lib/left.ts"),("Base","lib/right.ts"),("Exact","lib/left.ts"),("Specific","lib/right.ts"),("Inherited","lib/left.ts")] {
            assert_eq!(outgoing(&state, caller)?[0].target_id, NodeId::symbol("function",None,path,"save",1).0, "{caller}");
        }
        let caller_before = state.graph.get_node("tsmodules", "file:app.ts")?.unwrap();
        process_ingest_stats(&state, "tsmodules", 2, &stats(&[("tsconfig.json", r#"{"compilerOptions":{"baseUrl":".","paths":{"@lib/*":["lib/right"]}}}"#)])).await?;
        state.graph.resolve_symbol_edges("tsmodules")?;
        assert_eq!(outgoing(&state, "Mapped")?[0].target_id, "sym:function:lib/right.ts:save:1");
        assert_eq!(outgoing(&state, "Inherited")?[0].target_id, "sym:function:lib/left.ts:save:1", "Nested config keeps its own inherited paths");
        assert_eq!(bincode::serialize(&caller_before)?, bincode::serialize(&state.graph.get_node("tsmodules", "file:app.ts")?.unwrap())?);
        process_ingest_stats(&state, "tsmodules", 3, &stats(&[("config/base.json", r#"{"compilerOptions":{"paths":{"@/*":["../lib/right"]}}}"#)])).await?;
        state.graph.resolve_symbol_edges("tsmodules")?;
        assert_eq!(outgoing(&state, "Inherited")?[0].target_id, "sym:function:lib/right.ts:save:1");
        Ok(())
    }.await;
    temporary.close()?;
    result
}

#[tokio::test]
async fn typescript_modules_invalid_or_cyclic_configs_do_not_fall_back_to_names()
-> anyhow::Result<()> {
    let temporary = tempfile::tempdir()?;
    let result = async {
        let state = state(temporary.path())?;
        process_ingest_stats(
            &state,
            "tsmodules",
            1,
            &stats(&[
                ("tsconfig.json", r#"{"extends":"./other.json"}"#),
                ("other.json", r#"{"extends":"./tsconfig.json"}"#),
                (
                    "app.ts",
                    "import { save } from '@lib'; function Run() { save(); }",
                ),
                ("decoy.ts", "export function save() {}"),
            ]),
        )
        .await?;
        state.graph.resolve_symbol_edges("tsmodules")?;
        let edges = outgoing(&state, "Run")?;
        assert!(
            edges[0]
                .target_id
                .starts_with(engram_core::typescript_modules::UNBOUND_PREFIX)
        );
        assert_eq!(
            edges[0].metadata.as_ref().unwrap()["typescript_module_bindings_v1"][0]["reason"],
            "cyclic_or_deep_config"
        );
        process_ingest_stats(
            &state,
            "tsmodules",
            2,
            &stats(&[("tsconfig.json", "{ /* unfinished")]),
        )
        .await?;
        state.graph.resolve_symbol_edges("tsmodules")?;
        let edges = outgoing(&state, "Run")?;
        assert!(
            edges[0]
                .target_id
                .starts_with(engram_core::typescript_modules::UNBOUND_PREFIX)
        );
        assert_eq!(
            edges[0].metadata.as_ref().unwrap()["typescript_module_bindings_v1"][0]["reason"],
            "unsupported_or_invalid_config"
        );
        Ok(())
    }
    .await;
    temporary.close()?;
    result
}
