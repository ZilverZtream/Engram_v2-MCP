#![allow(clippy::unwrap_used)]
//! External audit round 2 (docs/audits/10, P0-4), live r39 after the callee
//! arm shipped: correctness fell 32 → 29 because the hop's graph items (seven
//! same-file callees of redovisninglista.vb, five of marker_edit.aspx.vb)
//! crowded the 10-item cap — a history question lost its commit documents
//! although they were in the index, and "which table stores reporting
//! categories" kept ss_systemsettings.sql instead of the table the question
//! names. Precision guard: every evidence KIND the plan needs keeps one item
//! under the cap, and the modality/file reserves prefer the candidate that
//! carries the question's own words.

use engram_server::services::ask_engine::evidence::{Authority, EvidenceItem, EvidenceKind};
use engram_server::services::ask_engine::plan::Modality;
use engram_server::services::ask_engine::ranking;

fn item(id: &str, kind: EvidenceKind, path: &str, content: &str, relevance: f32) -> EvidenceItem {
    EvidenceItem {
        evidence_id: id.into(),
        kind,
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
fn a_needed_evidence_kind_survives_the_cap() {
    // The plan needs history evidence (a "when was … last changed" question);
    // the ranked set is ten code chunks; the raw pool holds the commit doc.
    let mut chosen: Vec<EvidenceItem> = (0..10)
        .map(|i| {
            item(
                &format!("c{i}"),
                EvidenceKind::SourceCode,
                &format!("Site/x{i}.vb"),
                "marker icon",
                0.9,
            )
        })
        .collect();
    let raw = {
        let mut r = chosen.clone();
        r.push(item(
            "h1",
            EvidenceKind::HistoryCommit,
            "commit:abc123",
            "custom marker icon upload",
            0.6,
        ));
        r
    };
    ranking::reserve_needed_kinds(&mut chosen, &raw, &[EvidenceKind::HistoryCommit]);
    assert!(
        chosen.iter().any(|e| e.kind == EvidenceKind::HistoryCommit),
        "the needed history item is kept under the cap: {:?}",
        chosen
            .iter()
            .map(|e| e.evidence_id.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        chosen.len(),
        10,
        "the cap holds — the weakest item made room"
    );
}

#[test]
fn the_modality_reserve_prefers_the_candidate_that_carries_the_questions_words() {
    let question = "Which table stores reporting categories (redovisningskategorier)?";
    let mut chosen: Vec<EvidenceItem> = (0..10)
        .map(|i| {
            item(
                &format!("c{i}"),
                EvidenceKind::SourceCode,
                &format!("Site/redovisninglista{i}.vb"),
                "redovisning",
                0.9,
            )
        })
        .collect();
    let raw = {
        let mut r = chosen.clone();
        r.push(item(
            "s1",
            EvidenceKind::SourceCode,
            "db-x.sql/dbo/Tables/ss_systemsettings.sql",
            "CREATE TABLE ss_systemsettings",
            0.8,
        ));
        r.push(item(
            "s2",
            EvidenceKind::SourceCode,
            "db-x.sql/dbo/Tables/rk_redovisningskategorier.sql",
            "CREATE TABLE rk_redovisningskategorier",
            0.5,
        ));
        r
    };
    ranking::reserve_modalities(&mut chosen, &raw, &[Modality::Sql], question);
    assert!(
        chosen.iter().any(|e| e
            .path
            .as_deref()
            .is_some_and(|p| p.ends_with("rk_redovisningskategorier.sql"))),
        "the .sql that carries the question's own word wins over a higher-relevance stranger: {:?}",
        chosen
            .iter()
            .filter_map(|e| e.path.clone())
            .collect::<Vec<_>>()
    );
}
