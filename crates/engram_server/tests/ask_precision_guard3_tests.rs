#![allow(clippy::unwrap_used)]
//! External audit round 2 (docs/audits/10, P0-4), live r41 (33/35): "Which
//! table stores reporting categories" cites ONE .sql among ten items —
//! precision 0.40 — because the modality reserve keeps a single item. A
//! question that names a modality deserves several of its candidates: up to
//! three items of the requested modality are reserved (the pool permitting),
//! preferring the ones that carry the question's words.

use engram_server::services::ask_engine::evidence::{Authority, EvidenceItem, EvidenceKind};
use engram_server::services::ask_engine::plan::Modality;
use engram_server::services::ask_engine::ranking;

fn item(id: &str, path: &str, content: &str, relevance: f32) -> EvidenceItem {
    EvidenceItem {
        evidence_id: id.into(),
        kind: EvidenceKind::SourceCode,
        authority: Authority::CurrentCode,
        path: Some(path.into()),
        lines: None,
        symbol_id: None,
        title: None,
        content: content.into(),
        generation: Some(1),
        commit: None,
        timestamp: None,
        confidence: 0.9,
        relevance,
        extraction_method: "fts".into(),
        warnings: Vec::new(),
        provider: "test".into(),
        score: None,
        directness: None,
    }
}

#[test]
fn a_requested_modality_gets_up_to_three_slots() {
    let question = "Which table stores reporting categories (redovisningskategorier)?";
    let mut chosen: Vec<EvidenceItem> = (0..10)
        .map(|i| {
            item(
                &format!("c{i}"),
                &format!("Site/App_Code/redovisning{i}.vb"),
                "redovisning",
                0.9,
            )
        })
        .collect();
    let raw = {
        let mut r = chosen.clone();
        r.push(item(
            "s1",
            "db/Tables/rk_redovisningskategorier.sql",
            "CREATE TABLE rk_redovisningskategorier",
            0.5,
        ));
        r.push(item(
            "s2",
            "db/Tables/ss_systemsettings.sql",
            "CREATE TABLE ss_systemsettings",
            0.8,
        ));
        r.push(item(
            "s3",
            "db/Views/rlv_redovisninglastvecka.sql",
            "CREATE VIEW rlv_redovisninglastvecka",
            0.7,
        ));
        r.push(item(
            "s4",
            "db/Tables/sct_scheduled_task.sql",
            "CREATE TABLE sct_scheduled_task",
            0.6,
        ));
        r
    };
    ranking::reserve_required_with(&mut chosen, &raw, &[], &[Modality::Sql], &[], question);
    let sql: Vec<String> = chosen
        .iter()
        .filter_map(|e| e.path.clone())
        .filter(|p| p.ends_with(".sql"))
        .collect();
    assert!(
        sql.len() >= 3,
        "up to three items of the requested modality are reserved: {sql:?}"
    );
    assert!(
        sql.iter()
            .any(|p| p.ends_with("rk_redovisningskategorier.sql")),
        "the one carrying the question's word is among them: {sql:?}"
    );
    assert_eq!(chosen.len(), 10, "the cap holds");
}
