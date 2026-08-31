//! Batch 1 Fix A (doc 11 grind): a LOOKUP-shaped question — exactly one
//! resolved entity, no Usage/Impact/History intent — answers in a handful of
//! items. Live r57: exact-fact rows cited their one answer inside ten items
//! (seven sibling DALs) and failed the 0.5 precision gate.

use engram_server::services::ask_engine::plan::{
    EntityKind, EntityMention, Intent, ResolvedEntity,
};
use engram_server::services::ask_engine::ranking;
use engram_server::services::ask_engine::retrieval::Depth;

fn entity(canonical: &str) -> EntityMention {
    EntityMention {
        text: canonical.to_string(),
        guessed_kind: EntityKind::Symbol,
        resolved: vec![ResolvedEntity {
            kind: EntityKind::Symbol,
            canonical: canonical.to_string(),
            node_id: None,
            confidence: 1.0,
        }],
    }
}

#[test]
fn a_single_entity_lookup_gets_a_small_cap() {
    let ents = vec![entity("Site/a.vb")];
    let intents = vec![(Intent::Explain, 0.6f32)];
    assert_eq!(ranking::lookup_cap(&ents, &intents, Depth::Standard), 5);
}

#[test]
fn a_usage_question_keeps_the_full_cap() {
    let ents = vec![entity("Site/a.vb")];
    let intents = vec![(Intent::Usage, 0.6f32)];
    assert_eq!(ranking::lookup_cap(&ents, &intents, Depth::Standard), 10);
}

#[test]
fn a_multi_entity_question_keeps_the_full_cap() {
    let ents = vec![entity("Site/a.vb"), entity("Site/b.vb")];
    let intents = vec![(Intent::Explain, 0.6f32)];
    assert_eq!(ranking::lookup_cap(&ents, &intents, Depth::Standard), 10);
}

#[test]
fn an_unresolved_question_keeps_the_full_cap() {
    let ents = vec![EntityMention {
        text: "mystery".into(),
        guessed_kind: EntityKind::Unknown,
        resolved: vec![],
    }];
    let intents = vec![(Intent::Explain, 0.6f32)];
    assert_eq!(ranking::lookup_cap(&ents, &intents, Depth::Standard), 10);
}
