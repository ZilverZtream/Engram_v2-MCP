//! Exercise interrupted and conflicted imports through their real public path.
use super::*;
use std::time::Duration;

fn source_key() -> String {
    format!("quality_source:{}", blake3::hash(b"policy.json").to_hex())
}

async fn pending(state: &AppState, pid: &str, source: &str) {
    let hash = blake3::hash(source.as_bytes()).to_hex().to_string();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(raw) = state.registry.get_meta(pid, &source_key()).unwrap() {
                let receipt: serde_json::Value = serde_json::from_str(&raw).unwrap();
                if receipt["pending"] == true && receipt["source_hash"] == hash {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("The real import must durably enter its pending state");
}

fn request(pid: &str) -> IngestQualityGatesRequest {
    serde_json::from_value(json!({
        "project_id":pid,"source_path":"policy.json","source_type":"rules"
    }))
    .unwrap()
}

fn rules(state: &AppState, pid: &str) -> serde_json::Value {
    serde_json::to_value(state.registry.list_repo_rules(pid).unwrap()).unwrap()
}

async fn assert_withheld(state: &AppState, pid: &str) {
    let text = engram_server::services::project_service::inject_repo_rules(
        state,
        pid,
        &engram_core::RelPath::new("Site/maps/Widget.vb"),
        "source body",
    )
    .await;
    assert!(
        text.contains("INCOMPLETE") && text.contains("interrupted"),
        "{text}"
    );
    assert!(
        !text.contains("[Repo Constraint]"),
        "An interrupted import cannot enforce a partial policy: {text}"
    );
}

#[test]
fn an_interrupted_authored_refresh_stays_withheld_after_restart_and_retry_converges() {
    let temporary = tempfile::tempdir().unwrap();
    // Native merge workers must settle before owned-directory teardown on
    // normal return and unwinding.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (state, engram, pid) = build_at(temporary.path()).await;
        let cfg = (*state.cfg).clone();
        let root = temporary.path().join("proj");
        let source = root.join("policy.json");
        std::fs::write(&source, json!([{"rule":SCOPED_MANDATE}]).to_string()).unwrap();
        ingest_json(&engram, &pid, "policy.json", false).await;
        let original = rules(&state, &pid);

        let candidate = json!([{"rule":"Always retain CandidateProbe validation. [check: forbid=CandidateProbe]"}]).to_string();
        std::fs::write(&source, &candidate).unwrap();
        let engine = state.get_project_cached(&pid).unwrap().search;
        let writer = engine.create_bulk_writer_async(&pid, &tokio_util::sync::CancellationToken::new()).await.unwrap();
        let latest = json!([{"rule":"Always preserve LatestProbe ownership. [check: forbid=LatestProbe]"}]).to_string();
        {
        let importing = engram.handle_ingest_quality_gates(request(&pid));
        tokio::pin!(importing);
        tokio::select! {
            _ = pending(&state, &pid, &candidate) => {},
            result = &mut importing => panic!("Import finished before the held commit was released: {result:?}"),
        }
        assert_withheld(&state, &pid).await;
        std::fs::write(&source, &latest).unwrap();
        writer.finish().unwrap();
        let error = importing.await.unwrap_err();
        assert!(error.message.contains("source changed during import") && error.message.contains("withheld"), "{error}");
        }
        assert_eq!(rules(&state, &pid), original, "Failed replacement must not partially retire or replace registry rules");
        assert_withheld(&state, &pid).await;
        drop(engine);
        drop(engram);
        drop(state);

        let (reopened, _events) = AppState::new(cfg).unwrap();
        let engram = Engram::new(reopened.clone());
        assert_withheld(&reopened, &pid).await;
        ingest_json(&engram, &pid, "policy.json", false).await;
        let current = reopened.registry.list_repo_rules(&pid).unwrap();
        assert_eq!(current.len(), 1);
        assert!(current[0].rule_text.contains("LatestProbe") && !current[0].rule_text.contains("CandidateProbe"));
        let receipt: serde_json::Value = serde_json::from_str(&reopened.registry.get_meta(&pid, &source_key()).unwrap().unwrap()).unwrap();
        assert_eq!(receipt["pending"], false);
        assert_eq!(receipt["source_hash"], blake3::hash(latest.as_bytes()).to_hex().to_string());
        let documents = reopened.get_project_cached(&pid).unwrap().search.list_docs_in_namespace(&pid, "quality_gate").unwrap();
        assert_eq!(documents.len(), 1, "Retry must clean abandoned and retired source evidence");
        let full = engram.handle_get_chunk(serde_json::from_value(json!({
            "project_id":pid,"namespace":"quality_gate","doc_id":documents[0].doc_id
        })).unwrap()).await.unwrap();
        assert!(full.content[0].as_text().unwrap().text.contains("LatestProbe"));
        assert!(!full.content[0].as_text().unwrap().text.contains("CandidateProbe"));
    });
    drop(runtime);
    temporary.close().unwrap();
}

#[test]
fn a_manual_rule_created_during_source_publication_survives_the_conflicted_import() {
    let temporary = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (state, engram, pid) = build_at(temporary.path()).await;
        let root = temporary.path().join("proj");
        let source = root.join("policy.json");
        std::fs::write(&source, json!([{"rule":SCOPED_MANDATE}]).to_string()).unwrap();
        ingest_json(&engram, &pid, "policy.json", false).await;
        let candidate = json!([
            {"rule":SCOPED_MANDATE},
            {"rule":"Always retain CandidateProbe validation. [check: forbid=CandidateProbe]"}
        ]).to_string();
        std::fs::write(&source, &candidate).unwrap();
        let parsed = engram_index::quality_gates::parse_quality_source_checked(
            &candidate, engram_index::quality_gates::QualitySource::CodingRulesMd, "policy.json"
        ).unwrap();
        let claimed_id = format!("qg-{}", parsed[1].id);
        let engine = state.get_project_cached(&pid).unwrap().search;
        let writer = engine.create_bulk_writer_async(&pid, &tokio_util::sync::CancellationToken::new()).await.unwrap();
        let importing = engram.handle_ingest_quality_gates(request(&pid));
        tokio::pin!(importing);
        tokio::select! {
            _ = pending(&state, &pid, &candidate) => {},
            result = &mut importing => panic!("Import finished before the held commit was released: {result:?}"),
        }
        engram.handle_add_repo_rule(serde_json::from_value(json!({
            "project_id":pid,"rule_id":claimed_id,"file_pattern":"Site/maps/**",
            "rule_text":"Manual ownership decision made after the source import captured its baseline."
        })).unwrap()).await.unwrap();
        let with_manual = rules(&state, &pid);
        writer.finish().unwrap();
        let error = importing.await.unwrap_err();
        assert!(error.message.contains("explicit adoption") && error.message.contains(&claimed_id)
            && error.message.contains("withheld"), "{error}");
        assert_eq!(rules(&state, &pid), with_manual, "Conflicted publication must commit no rule changes");
        let retry = engram.handle_ingest_quality_gates(request(&pid)).await.unwrap_err();
        assert!(retry.message.contains("explicit adoption") && retry.message.contains("No rules imported"), "{retry}");
        assert_eq!(rules(&state, &pid), with_manual, "Retry cannot overwrite the intervening manual decision");
        let receipt: serde_json::Value = serde_json::from_str(&state.registry.get_meta(&pid, &source_key()).unwrap().unwrap()).unwrap();
        assert_eq!(receipt["pending"], true);
    });
    drop(runtime);
    temporary.close().unwrap();
}
