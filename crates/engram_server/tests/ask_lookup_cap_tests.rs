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
fn a_where_defined_question_is_not_a_breadth_usage() {
    // Live r60 exact_3: "Where is CanUserBulkUpdate defined?" fired
    // Usage(0.8) from the bare "where is" cue and blocked the lookup cap —
    // a where-DEFINED question asks for a location, not for callers.
    let plan = engram_server::services::ask_engine::planner::plan_query(
        "Where is CanUserBulkUpdate defined?",
    );
    assert!(
        !plan.intents.iter().any(|(i, _)| matches!(i, Intent::Usage)),
        "where-defined must not classify as breadth Usage: {:?}",
        plan.intents
    );
}

#[test]
fn the_item_with_every_asked_term_outranks_a_single_term_swarm() {
    // Live r60 usage_4: five single-term FK rows filled the slots while the
    // one item exhibiting BOTH asked terms ranked ninth — corroboration
    // rewarded the swarm, not the intersection.
    use engram_server::services::ask_engine::evidence as ev;
    let mk = |id: &str, content: &str, relevance: f32, directness: f32| ev::EvidenceItem {
        evidence_id: id.to_string(),
        kind: ev::EvidenceKind::SourceCode,
        authority: ev::Authority::CurrentCode,
        path: Some(format!("{id}.vb")),
        lines: None,
        symbol_id: None,
        title: None,
        content: content.to_string(),
        generation: None,
        commit: None,
        timestamp: None,
        confidence: 0.8,
        relevance,
        extraction_method: "t".into(),
        warnings: vec![],
        provider: "t".into(),
        score: None,
        directness: Some(directness),
    };
    let items = vec![
        mk("y1", "foreign_key pr_id", 0.65, 0.85),
        mk("y2", "foreign_key pr_id", 0.65, 0.85),
        mk("y3", "foreign_key pr_id", 0.65, 0.85),
        mk(
            "x",
            "pr_id = GetDictionaryIntegerValue(qry.params)",
            0.0,
            0.7,
        ),
    ];
    let terms = vec!["getdictionaryintegervalue".to_string(), "pr_id".to_string()];
    let out = ranking::rank_and_select_with_terms(items, 3, &terms);
    assert_eq!(
        out[0].evidence_id, "x",
        "the all-terms item must rank first"
    );
}

#[test]
fn anchored_retain_keeps_only_items_that_mention_the_entity() {
    // Live r60 exact_3: under an engaged lookup cap, concept fillers that
    // never mention the asked entity spent three of five slots.
    use engram_server::services::ask_engine::evidence as ev;
    let mk = |id: &str, path: &str, content: &str| ev::EvidenceItem {
        evidence_id: id.to_string(),
        kind: ev::EvidenceKind::SourceCode,
        authority: ev::Authority::CurrentCode,
        path: Some(path.to_string()),
        lines: None,
        symbol_id: None,
        title: None,
        content: content.to_string(),
        generation: None,
        commit: None,
        timestamp: None,
        confidence: 0.8,
        relevance: 0.5,
        extraction_method: "t".into(),
        warnings: vec![],
        provider: "t".into(),
        score: Some(0.5),
        directness: Some(0.5),
    };
    let mut items = vec![
        mk(
            "a",
            "api-x.vb",
            "private shared function canuserbulkupdate()",
        ),
        mk("b", "junk1.vb", "unrelated concept"),
        mk("c", "junk2.vb", "another filler"),
    ];
    ranking::retain_entity_anchored(
        &mut items,
        &["canuserbulkupdate".to_string()],
        &Default::default(),
    );
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].evidence_id, "a");
    // Fail-safe: a needle that matches nothing must not empty the answer.
    let mut all_junk = vec![mk("d", "junk3.vb", "still unrelated")];
    ranking::retain_entity_anchored(&mut all_junk, &["zzz".to_string()], &Default::default());
    assert_eq!(all_junk.len(), 1);
    // Batch 4c: a graph-relation item (entity-seeded hop) survives even
    // when its text never repeats the entity's name.
    let mut rel = vec![
        mk(
            "a",
            "api-x.vb",
            "private shared function canuserbulkupdate()",
        ),
        {
            let mut r = mk("r", "api-orders.vb", "calls GetOrderLines");
            r.kind = ev::EvidenceKind::GraphRelation;
            r
        },
        mk("e", "junk4.vb", "filler"),
    ];
    ranking::retain_entity_anchored(
        &mut rel,
        &["canuserbulkupdate".to_string()],
        &Default::default(),
    );
    assert_eq!(
        rel.iter()
            .map(|i| i.evidence_id.as_str())
            .collect::<Vec<_>>(),
        vec!["a", "r"]
    );
}

#[test]
fn an_unresolved_junk_mention_does_not_flip_the_cooccurrence_switch() {
    // Live r61: "data-access" (resolved: []) joined the term list, the >=2
    // co-occurrence mode engaged, and prose chunks containing the junk word
    // took the direct-evidence boost — exact_2/exact_5 regressed.
    let ents = vec![
        entity("Redovisningskategorier"),
        EntityMention {
            text: "data-access".into(),
            guessed_kind: EntityKind::File,
            resolved: vec![],
        },
    ];
    let terms = ranking::cooccurrence_terms(&ents);
    assert_eq!(terms, vec!["redovisningskategorier".to_string()]);
}

#[test]
fn two_resolved_mentions_both_reach_the_term_list() {
    let ents = vec![entity("GetDictionaryIntegerValue"), entity("pr_id")];
    let terms = ranking::cooccurrence_terms(&ents);
    assert_eq!(
        terms.len(),
        2,
        "both resolved mentions must survive: {terms:?}"
    );
}

#[test]
fn a_reserve_protected_item_survives_the_trims() {
    // Sweep 71 / chain c43d: whichever order reserve and the trims ran in,
    // one side's guarantee was destroyed. The reserve's protected id-set is
    // now visible to both trims.
    use engram_server::services::ask_engine::evidence as ev;
    let mk = |id: &str, path: &str, content: &str| ev::EvidenceItem {
        evidence_id: id.to_string(),
        kind: ev::EvidenceKind::SourceCode,
        authority: ev::Authority::CurrentCode,
        path: Some(path.to_string()),
        lines: None,
        symbol_id: None,
        title: None,
        content: content.to_string(),
        generation: None,
        commit: None,
        timestamp: None,
        confidence: 0.8,
        relevance: 0.5,
        extraction_method: "t".into(),
        warnings: vec![],
        provider: "t".into(),
        score: Some(0.5),
        directness: Some(0.5),
    };
    let mut protected = std::collections::HashSet::new();
    protected.insert("rdl".to_string());
    let mut items = vec![
        mk("a", "api-x.vb", "canuserbulkupdate here"),
        mk("rdl", "report.rdl", "no needle text at all"),
    ];
    ranking::retain_entity_anchored(&mut items, &["canuserbulkupdate".to_string()], &protected);
    assert_eq!(items.len(), 2, "protected item must survive anchoring");
    let mut dup = vec![mk("a", "x.vb", "one"), mk("b", "x.vb", "two")];
    let mut prot2 = std::collections::HashSet::new();
    prot2.insert("b".to_string());
    ranking::retain_one_per_path(&mut dup, &prot2);
    assert!(
        dup.iter().any(|i| i.evidence_id == "b"),
        "protected survives the per-path collapse"
    );
}

#[test]
fn a_plural_caller_question_is_a_usage_question() {
    // Live r62 causal_13: "Which TypeScript files call X?" matched no Usage
    // cue (the rule wanted " calls ") — the callers arm never ran and two
    // score-0.00 fillers made 1/3 precision.
    let plan = engram_server::services::ask_engine::planner::plan_query(
        "Which TypeScript files call gisGetProjectImportData?",
    );
    assert!(
        plan.intents.iter().any(|(i, _)| matches!(i, Intent::Usage)),
        "plural-caller shape must classify as Usage: {:?}",
        plan.intents
    );
}

#[test]
fn a_source_symbol_outranks_its_derived_state_twin() {
    // Live r62 exact_5: CheckIfAdminOrArbetsledare resolved to the function
    // AND its session-cached state node — one file, not ambiguous.
    use engram_server::services::ask_engine::resolver;
    let mk = |node_id: &str| ResolvedEntity {
        kind: EntityKind::Symbol,
        canonical: "x".into(),
        node_id: Some(node_id.to_string()),
        confidence: 0.5,
    };
    let mut v = vec![
        mk("state:Session:aspnetUser.X"),
        mk("sym:function:a.vb:X:566"),
    ];
    resolver::collapse_derived_resolutions(&mut v);
    assert_eq!(v.len(), 1, "the source symbol alone survives");
    assert!(v[0].node_id.as_deref().unwrap().starts_with("sym:"));
    // No-op when there is nothing to prefer.
    let mut all_state = vec![mk("state:A"), mk("state:B")];
    resolver::collapse_derived_resolutions(&mut all_state);
    assert_eq!(all_state.len(), 2);
}

#[test]
fn a_directory_scope_becomes_a_path_qualifier() {
    // Live r63 usage_5: "under ts/map" minted an unresolvable entity while
    // retrieval wandered — a directory-shaped token after a locative
    // preposition is a PATH SCOPE.
    let plan = engram_server::services::ask_engine::planner::plan_query(
        "Which TypeScript files under ts/map render the camera icon on a marker?",
    );
    assert_eq!(
        plan.qualifiers.path_prefixes,
        vec!["ts/map".to_string()],
        "the scope must be extracted: {:?}",
        plan.qualifiers
    );
}

#[test]
fn path_scoped_retain_keeps_only_the_scope_and_never_empties() {
    use engram_server::services::ask_engine::evidence as ev;
    let mk = |id: &str, path: &str| ev::EvidenceItem {
        evidence_id: id.to_string(),
        kind: ev::EvidenceKind::SourceCode,
        authority: ev::Authority::CurrentCode,
        path: Some(path.to_string()),
        lines: None,
        symbol_id: None,
        title: None,
        content: String::new(),
        generation: None,
        commit: None,
        timestamp: None,
        confidence: 0.8,
        relevance: 0.5,
        extraction_method: "t".into(),
        warnings: vec![],
        provider: "t".into(),
        score: Some(0.5),
        directness: Some(0.5),
    };
    let scope = vec!["ts/map".to_string()];
    let mut items = vec![
        mk("a", "Site/modules/dashboard/ts/map/vsMap/x.ts"),
        mk("b", "Site/App_Code/users-security/code/aspnetUsers.vb"),
    ];
    engram_server::services::ask_engine::ranking::retain_path_scoped(&mut items, &scope);
    assert_eq!(
        items
            .iter()
            .map(|i| i.evidence_id.as_str())
            .collect::<Vec<_>>(),
        vec!["a"]
    );
    // Fail-safe: a scope nothing matches must not empty the pool.
    let mut none = vec![mk("c", "Site/App_Code/y.vb")];
    engram_server::services::ask_engine::ranking::retain_path_scoped(
        &mut none,
        &["zz/qq".to_string()],
    );
    assert_eq!(none.len(), 1);
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
fn one_item_per_path_after_the_lookup_trim() {
    // Live r59 exact_6: 2/5 relevant with a same-file pair filling two slots.
    use engram_server::services::ask_engine::evidence as ev;
    let mk = |id: &str, path: &str| ev::EvidenceItem {
        evidence_id: id.to_string(),
        kind: ev::EvidenceKind::SourceCode,
        authority: ev::Authority::CurrentCode,
        path: Some(path.to_string()),
        lines: None,
        symbol_id: None,
        title: None,
        content: String::new(),
        generation: None,
        commit: None,
        timestamp: None,
        confidence: 0.8,
        relevance: 0.5,
        extraction_method: "t".into(),
        warnings: vec![],
        provider: "t".into(),
        score: Some(0.5),
        directness: Some(0.5),
    };
    let mut items = vec![mk("a", "x.vb"), mk("b", "x.vb"), mk("c", "y.vb")];
    ranking::retain_one_per_path(&mut items, &Default::default());
    assert_eq!(
        items
            .iter()
            .map(|i| i.evidence_id.as_str())
            .collect::<Vec<_>>(),
        vec!["a", "c"]
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
