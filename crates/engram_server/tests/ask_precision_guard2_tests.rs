#![allow(clippy::unwrap_used)]
//! External audit round 2 (docs/audits/10, P0-4), live r40 (31/35): three
//! precise misses. (1) "Which files change together with text.resx" lost its
//! .resx item because the needed-kind reserve evicted the LAST item of the
//! set — the one the modality reserve had just pushed: the reserves must run
//! as ONE pass that protects what it reserved. (2) "Which table stores
//! reporting categories" cited the same .vb five times (precision 0.40): at
//! most two items per file. (3) "how are permission checks done in
//! api-installationsobjektprojekt" never extracted the lowercase hyphenated
//! file name as an entity, so nothing could reserve it.

use engram_server::services::ask_engine::evidence::{Authority, EvidenceItem, EvidenceKind};
use engram_server::services::ask_engine::plan::{EntityKind, Modality};
use engram_server::services::ask_engine::{planner, ranking};

fn item(
    id: &str,
    kind: EvidenceKind,
    path: &str,
    lines: Option<(u32, u32)>,
    content: &str,
    relevance: f32,
) -> EvidenceItem {
    EvidenceItem {
        evidence_id: id.into(),
        kind,
        authority: Authority::CurrentCode,
        path: Some(path.into()),
        lines,
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
fn one_reserve_pass_never_evicts_what_another_reserve_just_kept() {
    let question = "Which files change together with text.resx when a resource key is added?";
    let mut chosen: Vec<EvidenceItem> = (0..10)
        .map(|i| {
            item(
                &format!("c{i}"),
                EvidenceKind::SourceCode,
                &format!("Site/x{i}.vb"),
                None,
                "resource key",
                0.9,
            )
        })
        .collect();
    let raw = {
        let mut r = chosen.clone();
        r.push(item(
            "r1",
            EvidenceKind::SourceCode,
            "Site/App_GlobalResources/text.resx",
            None,
            "resource key text",
            0.7,
        ));
        r.push(item(
            "h1",
            EvidenceKind::HistoryCommit,
            "commit:abc123",
            None,
            "text.resx resource key added",
            0.6,
        ));
        r
    };
    ranking::reserve_required_with(
        &mut chosen,
        &raw,
        &[EvidenceKind::HistoryCommit],
        &[Modality::Resource],
        &[],
        question,
    );
    let paths: Vec<String> = chosen.iter().filter_map(|e| e.path.clone()).collect();
    assert!(
        paths.iter().any(|p| p.ends_with(".resx")),
        "the .resx reserve survives the history reserve: {paths:?}"
    );
    assert!(
        chosen.iter().any(|e| e.kind == EvidenceKind::HistoryCommit),
        "the history reserve is kept too: {paths:?}"
    );
    assert_eq!(
        chosen.len(),
        10,
        "the cap holds — two weakest UNPROTECTED items made room"
    );
}

#[test]
fn ranking_cites_at_most_two_items_per_file() {
    let mut items: Vec<EvidenceItem> = (0..5)
        .map(|i| {
            item(
                &format!("same{i}"),
                EvidenceKind::SourceCode,
                "Site/App_Code/redovisninglista.vb",
                Some((i * 200 + 1, i * 200 + 40)),
                "redovisningskategorier lista",
                0.95,
            )
        })
        .collect();
    for i in 0..8 {
        items.push(item(
            &format!("other{i}"),
            EvidenceKind::SourceCode,
            &format!("Site/App_Code/other{i}.vb"),
            Some((1, 30)),
            "redovisningskategorier",
            0.6,
        ));
    }
    let chosen = ranking::rank_and_select(items, 10);
    let same = chosen
        .iter()
        .filter(|e| e.path.as_deref() == Some("Site/App_Code/redovisninglista.vb"))
        .count();
    assert!(
        same <= 2,
        "one file may not fill the evidence set: {same} items from the same file"
    );
    assert_eq!(chosen.len(), 10, "the cap is still filled with other files");
}

#[test]
fn a_lowercase_hyphenated_file_name_is_an_entity_mention() {
    let plan = planner::plan_query(
        "How are permission checks done in api-installationsobjektprojekt, and which endpoints read a client-supplied project id?",
    );
    let m = plan
        .entities
        .iter()
        .find(|e| {
            e.text
                .eq_ignore_ascii_case("api-installationsobjektprojekt")
        })
        .unwrap_or_else(|| {
            panic!(
                "the hyphenated file name is a mention: {:?}",
                plan.entities
                    .iter()
                    .map(|e| e.text.clone())
                    .collect::<Vec<_>>()
            )
        });
    assert!(matches!(m.guessed_kind, EntityKind::File), "{m:?}");
}
