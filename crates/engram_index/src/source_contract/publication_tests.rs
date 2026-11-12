//! Explicit serialized proof with an actual loaded companion. This establishes
//! code/policy reuse and the synchronous registry fence, not compilation inputs.
use super::*;
use anyhow::{Result, ensure};
use crate::vb_extractor::{self, VbFallbackReason, VbParserProvenance};

#[test]
fn script_reuse_requires_produced_receiver_contract_without_invalidating_other_languages() -> Result<()> {
    let expected = SourceExtractionExpectation::for_paths(2000, ["owned.ts"])?;
    let legacy = serde_json::json!({"source_index_version":crate::SOURCE_INDEX_VERSION,
        "source_extraction_identity":SourceExtractionIdentity::for_chunk_target(2000)});
    for path in ["owned.ts", "view.TSX", "plain.js", "empty.jsx"] {
        ensure!(!expected.metadata_is_compatible(&legacy, path));
        for unsupported in [serde_json::Value::Null, serde_json::json!(0), serde_json::json!(2), serde_json::json!("1")] {
            let mut metadata = legacy.clone(); metadata["script_call_contract"] = unsupported;
            ensure!(!expected.metadata_is_compatible(&metadata, path));
        }
        let mut metadata = legacy.clone(); metadata["script_call_contract"] = serde_json::json!(SCRIPT_CALL_CONTRACT);
        if matches!(path, "owned.ts" | "view.TSX") {
            ensure!(!expected.metadata_is_compatible(&metadata, path), "Legacy receiver facts do not establish module identity");
            let (symbols, _) = crate::parsing::SymbolExtractor::new().extract(std::path::Path::new(path), "export function run() {}");
            let facts = symbols.iter().find(|s| s.kind == "file").unwrap().metadata.as_ref().unwrap();
            for (key, value) in facts { metadata[key] = serde_json::json!(value); }
        }
        ensure!(expected.metadata_is_compatible(&metadata, path));
    }
    for path in ["owned.go", "style.css", "page.html", "server.cs"] {
        ensure!(expected.metadata_is_compatible(&legacy, path));
    }
    let mut output = crate::IngestStats {
        source_extraction_identity: Some(SourceExtractionIdentity::for_chunk_target(2000)),
        files:1, all_files:vec![engram_core::RelPath::new("owned.ts")], ..Default::default()
    };
    ensure!(expected.verify_produced(&output).is_err(), "Cannot publish an unreceipted script parse");
    output.script_call_contracts.insert("owned.ts".into(), SCRIPT_CALL_CONTRACT);
    ensure!(expected.verify_produced(&output).is_err(), "TypeScript module facts must be produced, not merely a receiver receipt");
    let (symbols, _) = crate::parsing::SymbolExtractor::new().extract(std::path::Path::new("owned.ts"), "export function run() {}");
    output.symbols.extend(symbols.into_iter().map(|s| (std::sync::Arc::new(engram_core::RelPath::new("owned.ts")), s)));
    expected.verify_produced(&output)?;
    output.script_call_contracts.insert("inherited.ts".into(), SCRIPT_CALL_CONTRACT);
    ensure!(expected.verify_produced(&output).is_err(), "Do not claim a parse for inherited source");
    output.script_call_contracts.remove("inherited.ts");
    output.script_call_contracts.insert("owned.ts".into(), SCRIPT_CALL_CONTRACT + 1);
    ensure!(expected.verify_produced(&output).is_err());
    Ok(())
}

#[test]
fn typescript_config_reuse_requires_extracted_facts_even_for_unchanged_json() -> Result<()> {
    let expected = SourceExtractionExpectation::for_paths(2000, ["tsconfig.json"])?;
    let mut metadata = serde_json::json!({"source_index_version":crate::SOURCE_INDEX_VERSION,
        "source_extraction_identity":SourceExtractionIdentity::for_chunk_target(2000)});
    ensure!(!expected.metadata_is_compatible(&metadata, "tsconfig.json"));
    let (symbols, _) = crate::parsing::SymbolExtractor::new().extract(std::path::Path::new("tsconfig.json"), r#"{"compilerOptions":{"paths":{"@/*":["src/*"]}}}"#);
    for (key, value) in symbols[0].metadata.as_ref().unwrap() { metadata[key] = serde_json::json!(value); }
    ensure!(expected.metadata_is_compatible(&metadata, "tsconfig.json"));
    metadata[engram_core::typescript_modules::CONFIG_KEY] = serde_json::json!("{}");
    ensure!(!expected.metadata_is_compatible(&metadata, "tsconfig.json"));
    Ok(())
}

#[test]
fn rust_modules_upgrade_legacy_source_and_manifest_facts_before_publication() -> Result<()> {
    for (path, source, key) in [
        ("src/lib.rs", "mod api; use api::save; pub fn run() { save(); }", engram_core::rust_modules::MODULE_KEY),
        ("Cargo.toml", "[package]\nname = \"owned\"\nversion = \"0.1.0\"\nedition = \"2021\"\n", engram_core::rust_modules::CONFIG_KEY),
    ] {
        let expected = SourceExtractionExpectation::for_paths(2000, [path])?;
        let mut metadata = serde_json::json!({"source_index_version":crate::SOURCE_INDEX_VERSION,
            "source_extraction_identity":SourceExtractionIdentity::for_chunk_target(2000)});
        ensure!(!expected.metadata_is_compatible(&metadata, path), "Legacy facts must be upgraded: {path}");
        let (symbols, _) = crate::parsing::SymbolExtractor::new().extract(std::path::Path::new(path), source);
        for (key, value) in symbols.iter().find(|s| s.kind == "file").unwrap().metadata.as_ref().unwrap() {
            metadata[key] = serde_json::json!(value);
        }
        ensure!(expected.metadata_is_compatible(&metadata, path));
        let mut output = crate::IngestStats {
            source_extraction_identity: Some(SourceExtractionIdentity::for_chunk_target(2000)),
            files: 1, all_files: vec![engram_core::RelPath::new(path)], ..Default::default()
        };
        ensure!(expected.verify_produced(&output).is_err(), "Publication requires produced facts: {path}");
        output.symbols.extend(symbols.into_iter().map(|s| (std::sync::Arc::new(engram_core::RelPath::new(path)), s)));
        expected.verify_produced(&output)?;
        metadata[key] = serde_json::json!("{}");
        ensure!(!expected.metadata_is_compatible(&metadata, path));
    }
    Ok(())
}

struct ParserOwner;

#[test]
fn python_modules_upgrade_legacy_facts_and_require_actual_production() -> Result<()> {
    let expected = SourceExtractionExpectation::for_paths(2000, ["app.py"])?;
    let mut metadata = serde_json::json!({"source_index_version":crate::SOURCE_INDEX_VERSION,
        "source_extraction_identity":SourceExtractionIdentity::for_chunk_target(2000)});
    ensure!(!expected.metadata_is_compatible(&metadata, "app.py"));
    ensure!(expected.metadata_is_compatible(&metadata, "app.go"));
    let (symbols, _) = crate::parsing::SymbolExtractor::new().extract(std::path::Path::new("app.py"),
        "from api import save\ndef run():\n    return save()\n");
    for (key,value) in symbols.iter().find(|s|s.kind=="file").unwrap().metadata.as_ref().unwrap() {
        metadata[key]=serde_json::json!(value);
    }
    ensure!(expected.metadata_is_compatible(&metadata, "app.PY"));
    let mut output=crate::IngestStats {
        source_extraction_identity:Some(SourceExtractionIdentity::for_chunk_target(2000)),
        files:1,all_files:vec![engram_core::RelPath::new("app.py")],..Default::default()
    };
    ensure!(expected.verify_produced(&output).is_err());
    output.symbols.extend(symbols.into_iter().map(|s|(std::sync::Arc::new(engram_core::RelPath::new("app.py")),s)));
    expected.verify_produced(&output)?;
    metadata[engram_core::python_modules::MODULE_KEY]=serde_json::json!("{}");
    ensure!(!expected.metadata_is_compatible(&metadata,"app.py"));
    Ok(())
}
impl Drop for ParserOwner {
    fn drop(&mut self) {
        if let Err(error) = vb_extractor::shutdown_idle_parser() {
            tracing::error!(error=%error, "Owned parser context fixture cleanup failed");
        }
    }
}

#[test]
#[ignore = "requires identity-capable ENGRAM_VB_SIDECAR_PATH, source cap <=4MiB, and serialized idle-parser ownership"]
fn real_vb_stable_degradation_reuses_context_and_policy_fence_protects_registry_publication() -> Result<()> {
    vb_extractor::shutdown_idle_parser()?;
    let owned = tempfile::tempdir()?;
    let parser_owner = ParserOwner;
    let result = (|| -> Result<()> {
        let expected = SourceExtractionExpectation::for_paths(2000, ["Owned.vb"])?;
        let context = expected.vb_parser.as_ref().ok_or_else(|| anyhow::anyhow!("Missing VB context"))?;
        ensure!(context.roslyn.is_some(), "This proof requires an actually verified loaded Roslyn child");
        ensure!(context.max_source_bytes <= 4 * 1024 * 1024, "Keep this fixture's source allocation bounded");
        let source = format!("Public Class Stable\nPublic Sub Run()\nEnd Sub\nEnd Class\n'{}\n", "x".repeat(context.max_source_bytes + 1));
        let path = owned.path().join("Owned.vb");
        std::fs::write(&path, &source)?;
        let (_, _, actual) = vb_extractor::extract_vb_for_ingestion(&path, &source)?;
        ensure!(matches!(&actual, VbParserProvenance::RustFallback { reason: VbFallbackReason::SourceLimit, .. }),
            "The real effective cap must select labelled fallback without stopping the healthy parser");
        let mut stats = crate::IngestStats {
            source_extraction_identity: Some(SourceExtractionIdentity::for_chunk_target(2000)),
            files: 1, bytes: source.len() as u64,
            all_files: vec![engram_core::RelPath::new("Owned.vb")], ..Default::default()
        };
        stats.vb_parser_provenance.insert("Owned.vb".into(), actual.clone());
        let metadata = serde_json::json!({
            "source_index_version":crate::SOURCE_INDEX_VERSION,
            "source_extraction_identity":stats.source_extraction_identity,
            "vb_parser_provenance":actual,
        });
        let registry = engram_core::Registry::open(&owned.path().join("registry.redb"))?;
        registry.register_source_project(&engram_core::ProjectRecord {
            project_id:"owned".into(), project_name:"Parser gate fixture".into(), project_type:"general".into(),
            directory:owned.path().to_string_lossy().into(), created_at_ms:1, updated_at_ms:1,
            reindex_required_since_ms:None,
        })?;
        let base = registry.capture_source_head("owned")?;
        for _ in 0..3 {
            let observed = SourceExtractionExpectation::for_paths(2000, ["Owned.vb"])?;
            ensure!(observed.metadata_is_compatible(&metadata, "Owned.vb"),
                "Same effective available code and stable degraded backend must not request perpetual migration");
            observed.verify_produced(&stats)?;
            ensure!(registry.source_head_is_current(&base)?, "Read-only compatibility must not advance source metadata");
        }
        let mut stale_policy = expected.clone();
        stale_policy.vb_parser.as_mut().unwrap().max_source_bytes += 1;
        ensure!(!stale_policy.metadata_is_compatible(&metadata, "Owned.vb"), "Changed effective policy must require migration");
        ensure!(stale_policy.verify_produced(&stats).is_err(), "Output must match the original scan, not just its final current parser");
        let called = std::cell::Cell::new(false);
        let refused = stale_policy.with_publication_fence(|| {
            called.set(true);
            registry.publish_source_head(&base, 1)
        });
        ensure!(refused.is_err() && !called.get() && registry.source_head_is_current(&base)?,
            "A stale parser policy must refuse before invoking the authoritative registry CAS");
        ensure!(expected.with_publication_fence(|| {
            ensure!(vb_extractor::current_parser_context().is_err(),
                "The actual parser coordinator must remain held through the registry closure");
            registry.publish_source_head(&base, 1)
        })?, "The original admitted context may commit its source head");
        drop(registry);
        Ok(())
    })();
    let stopped = vb_extractor::shutdown_idle_parser();
    drop(parser_owner);
    let cleanup = owned.close();
    result?;
    stopped?;
    cleanup?;
    Ok(())
}
