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
fn a_junk_unresolved_mention_does_not_block_the_small_cap() {
    // Live r58: the planner minted "data-access" (resolved: []) beside the
    // real entity — the strict entities.len()==1 condition never fired.
    let ents = vec![
        entity("Site/a.vb"),
        EntityMention {
            text: "data-access".into(),
            guessed_kind: EntityKind::File,
            resolved: vec![],
        },
    ];
    let intents = vec![(Intent::Explain, 0.6f32)];
    assert_eq!(ranking::lookup_cap(&ents, &intents, Depth::Standard), 5);
}

#[test]
fn a_long_lowercase_file_stem_word_is_minted_as_a_mention() {
    // Live r58 (ox_exact_2 shape): "redovisningskategorier" IS a file stem in
    // the corpus, but only hyphenated lowercase tokens were minted — the
    // question's one real entity never reached the resolver.
    let ments = engram_server::services::ask_engine::planner::extract_entities(
        "Which file defines the redovisningskategorier data-access class?",
    );
    assert!(
        ments
            .iter()
            .any(|m| m.text.eq_ignore_ascii_case("redovisningskategorier")),
        "the long domain word must be minted; got {:?}",
        ments.iter().map(|m| m.text.as_str()).collect::<Vec<_>>()
    );
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
