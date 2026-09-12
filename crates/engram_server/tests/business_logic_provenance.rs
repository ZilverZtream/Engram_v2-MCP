use engram_core::Config;
use engram_ml::DreamingEngine;
use engram_server::services::business_logic_service::{
    analyze_method_logic, parse_llm_response, render_method_as_doc,
};

#[test]
fn effective_provider_identity_retains_defaults_and_is_not_mutable_config() {
    let mut config = Config {
        llm_backend: "ollama".into(),
        llm_provider: Some("openrouter".into()),
        llm_model: Some("vendor/requested".into()),
        llm_openai_api_key: Some("secret-fixture-key".into()),
        llm_openai_api_base: Some("https://secret.example/private".into()),
        ..Default::default()
    };
    let engine = DreamingEngine::with_config(&config);
    config.llm_model = Some("later-model".into());
    assert_eq!(
        engine.text_generation_identity(),
        Some(("openrouter", Some("vendor/requested")))
    );
    let later = DreamingEngine::with_config(&config);
    assert_eq!(
        later.text_generation_identity(),
        Some(("openrouter", Some("later-model")))
    );
    for (backend, model) in [
        ("ollama", "llama3.2"),
        ("openai", "gpt-4o-mini"),
        ("openrouter", "openai/gpt-4o-mini"),
    ] {
        let engine = DreamingEngine::with_config(&Config {
            llm_backend: backend.into(),
            ..Default::default()
        });
        assert_eq!(
            engine.text_generation_identity(),
            Some((backend, Some(model)))
        );
    }
    assert_eq!(DreamingEngine::new().text_generation_identity(), None);
}

#[tokio::test]
async fn deterministic_and_unavailable_origins_are_not_mislabeled_as_llm_outputs() {
    let engine = DreamingEngine::with_config(&Config {
        llm_backend: "openai".into(),
        llm_model: Some("configured-but-unused".into()),
        ..Default::default()
    });
    let empty = analyze_method_logic(
        &engine,
        "Rules.vb",
        "Save",
        "Sub Save()\nEnd Sub",
        "Rules",
        "vb",
        1,
    )
    .await;
    let provenance = empty.extraction_provenance.as_ref().unwrap();
    assert_eq!(provenance.origin, "deterministic");
    assert!(provenance.provider.is_none());
    assert!(provenance.requested_model.is_none());
    assert!(provenance.prompt_version.is_none());
    assert!(!render_method_as_doc(&empty).contains("configured-but-unused"));
    let unavailable = analyze_method_logic(
        &DreamingEngine::new(),
        "Rules.vb",
        "Save",
        "Sub Save()\n Store()\nEnd Sub",
        "Rules",
        "vb",
        1,
    )
    .await;
    assert!(!unavailable.parse_diagnostic.is_empty());
    assert_eq!(
        unavailable.extraction_provenance.as_ref().unwrap().origin,
        "llm_unavailable"
    );
}

#[test]
fn external_or_legacy_analysis_has_unknown_provenance_not_current_configuration() {
    let analysis = parse_llm_response(
        r#"{"purpose":"Returns a value","business_rules":[]}"#,
        "Rules.vb",
        "Read",
        "Rules.Read",
        "hash",
    );
    assert!(analysis.extraction_provenance.is_none());
    assert!(render_method_as_doc(&analysis).contains("Extraction provenance**: unknown"));
    assert!(render_method_as_doc(&analysis).contains("**Analysis method hash**: `hash`"));
}
