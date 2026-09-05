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
fn an_infix_scope_expands_to_the_real_prefix() {
    // Live r64 usage_5: the engine's include_path_prefixes is an ANCHORED
    // regex — "ts/map" can never match "Site/modules/dashboard/ts/map/…".
    use engram_server::services::ask_engine::resolver;
    assert_eq!(
        resolver::scope_full_prefix("Site/modules/dashboard/ts/map/vsMap/x.ts", "ts/map"),
        Some("Site/modules/dashboard/ts/map".to_string())
    );
    assert_eq!(
        resolver::scope_full_prefix("ts/map/a.ts", "ts/map"),
        Some("ts/map".to_string())
    );
    assert_eq!(
        resolver::scope_full_prefix("Site/TS/Map/x.ts", "ts/map"),
        Some("Site/TS/Map".to_string())
    );
    assert_eq!(
        resolver::scope_full_prefix("Site/other/x.ts", "ts/map"),
        None
    );
}

#[test]
fn a_language_name_is_covered_by_evidence_in_that_language() {
    // Live r65 causal_1/usage_5: "TypeScript" was an uncovered named premise
    // — .ts files ARE TypeScript; the literal word rarely appears in code,
    // and status oscillated with whether any junk item happened to say it.
    use engram_server::services::ask_engine::evidence as ev;
    use engram_server::services::ask_engine::status;
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
    let items = vec![mk(
        "a",
        "Site/ts/caw/caw.ts",
        "public athDeleteByID(id: number): void",
    )];
    let unc = status::uncovered_named_terms_with(
        "Which TypeScript code calls the athDeleteByID API?",
        &items,
        &[],
    );
    assert!(
        !unc.iter().any(|t| t.eq_ignore_ascii_case("typescript")),
        "a .ts item covers the language name: {unc:?}"
    );
    // The fabricated-premise guard stays: "Redis" with no evidence remains
    // uncovered.
    let unc2 = status::uncovered_named_terms_with(
        "Which Redis cluster caches the marker list?",
        &items,
        &[],
    );
    assert!(
        unc2.iter().any(|t| t.eq_ignore_ascii_case("redis")),
        "a real named premise must still require coverage: {unc2:?}"
    );
    // Live r65 multi_1: a tech ROLE acronym is vocabulary, not a premise.
    let unc3 = status::uncovered_named_terms_with(
        "How does the update flow from the API to the DAL?",
        &items,
        &[],
    );
    assert!(
        !unc3.iter().any(|t| t.eq_ignore_ascii_case("dal")),
        "a tech role word must not veto support: {unc3:?}"
    );
}

#[test]
fn a_named_file_callee_question_contracts_an_exhaustive_function_set() {
    // Doc-12 P0-2 probe shape: "Which server API functions does X call?" —
    // the answer is a SET of functions; caps and one-per-file dedup must
    // not silently shrink it (Phases B–D key off this contract).
    use engram_server::services::ask_engine::plan::{
        Cardinality, ContractDirection, ContractEntityType,
    };
    let plan = engram_server::services::ask_engine::planner::plan_query(
        "Which server API functions does orderPanel.ts call?",
    );
    let c = &plan.contract;
    assert!(matches!(c.direction, ContractDirection::Callees), "{c:?}");
    assert!(matches!(c.cardinality, Cardinality::ExhaustiveSet), "{c:?}");
    assert!(
        matches!(c.entity_type, ContractEntityType::Function),
        "{c:?}"
    );
    assert!(c.completeness_required, "{c:?}");
}

#[test]
fn a_plural_files_question_contracts_a_file_set() {
    // Doc-12 P0-1 probe shape: "Which TypeScript files under ts/map render
    // the camera icon?" — a file SET with completeness required.
    use engram_server::services::ask_engine::plan::{Cardinality, ContractEntityType};
    let plan = engram_server::services::ask_engine::planner::plan_query(
        "Which TypeScript files under ts/map render the camera icon on a marker?",
    );
    let c = &plan.contract;
    assert!(matches!(c.entity_type, ContractEntityType::File), "{c:?}");
    assert!(matches!(c.cardinality, Cardinality::ExhaustiveSet), "{c:?}");
    assert!(c.completeness_required, "{c:?}");
}

#[test]
fn a_plural_callers_question_contracts_an_exhaustive_caller_set() {
    use engram_server::services::ask_engine::plan::{
        Cardinality, ContractDirection, ContractEntityType,
    };
    let plan = engram_server::services::ask_engine::planner::plan_query(
        "Which TypeScript files call gisGetProjectImportData?",
    );
    let c = &plan.contract;
    assert!(matches!(c.direction, ContractDirection::Callers), "{c:?}");
    assert!(matches!(c.cardinality, Cardinality::ExhaustiveSet), "{c:?}");
    assert!(matches!(c.entity_type, ContractEntityType::File), "{c:?}");
    assert!(c.completeness_required, "{c:?}");
}

#[test]
fn a_where_defined_question_contracts_one_definition() {
    use engram_server::services::ask_engine::plan::{Cardinality, Facet};
    let plan = engram_server::services::ask_engine::planner::plan_query(
        "Where is CanUserBulkUpdate defined?",
    );
    let c = &plan.contract;
    assert!(matches!(c.cardinality, Cardinality::One), "{c:?}");
    assert!(c.required_facets.contains(&Facet::Definition), "{c:?}");
    assert!(!c.completeness_required, "{c:?}");
}

#[test]
fn an_explain_question_keeps_the_default_contract() {
    // No false exhaustiveness: an open "how does …" question stays TopK.
    use engram_server::services::ask_engine::plan::{Cardinality, ContractDirection};
    let plan = engram_server::services::ask_engine::planner::plan_query(
        "How does the map's marker info window fetch a marker's images?",
    );
    let c = &plan.contract;
    assert!(matches!(c.direction, ContractDirection::None), "{c:?}");
    assert!(matches!(c.cardinality, Cardinality::TopK), "{c:?}");
    assert!(!c.completeness_required, "{c:?}");
}

#[test]
fn arm_coverage_travels_on_the_outcome() {
    // Doc-12 P1 (invisible caps): every arm reports what it examined, what
    // was available, and whether it truncated — zeros mean "not measured",
    // never "complete".
    use engram_server::services::ask_engine::providers::{ArmCoverage, ProviderOutcome};
    let o = ProviderOutcome::hit_with_coverage(25, Some(40), true);
    assert_eq!(o.coverage.examined, 25);
    assert_eq!(o.coverage.available, Some(40));
    assert!(o.coverage.truncated);
    let z = ProviderOutcome::hit();
    assert_eq!(z.coverage, ArmCoverage::default());
    let t = ProviderOutcome::timed_out();
    assert_eq!(t.coverage, ArmCoverage::default());
}

#[test]
fn an_exhaustive_callee_set_keeps_every_function_of_a_shared_file() {
    // Doc-12 P0-2: many API functions live in ONE implementation file —
    // "one item per file" destroys the requested function cardinality.
    use engram_core::RelPath;
    use engram_graph::{Edge, EdgeKind, GraphStore, Node};
    let tmp = tempfile::tempdir().unwrap();
    let g = GraphStore::open(&tmp.path().join("g.redb")).unwrap();
    let mk_node = |id: &str, ty: &str, name: &str, path: &str| Node {
        node_id: id.to_string(),
        node_type: ty.to_string(),
        name: name.to_string(),
        namespace: "code".to_string(),
        language: "ts".to_string(),
        file_path: RelPath::new(path),
        start_line: 1,
        end_line: 5,
        generation: 1,
        metadata: None,
    };
    let mk_edge = |s: &str, t: &str, k: EdgeKind| Edge {
        source_id: s.to_string(),
        target_id: t.to_string(),
        namespace: "code".to_string(),
        language: "ts".to_string(),
        edge_kind: k,
        weight: 1,
        generation: 1,
        metadata: None,
        updated_at_ms: 1,
    };
    let ts = "site/ts/panel.ts";
    let vb = "site/api/api-shared.vb";
    let nodes = vec![
        mk_node("fn:a", "function", "loadA", ts),
        mk_node("fn:b", "function", "loadB", ts),
        mk_node("api:x", "function", "apiGetX", vb),
        mk_node("api:y", "function", "apiGetY", vb),
        mk_node("api:z", "function", "apiGetZ", vb),
    ];
    let edges = vec![
        mk_edge("fn:a", "api:x", EdgeKind::ApiCall),
        mk_edge("fn:a", "api:y", EdgeKind::ApiCall),
        mk_edge("fn:b", "api:z", EdgeKind::ApiCall),
    ];
    g.upsert_nodes_and_edges("p", &nodes, &edges).unwrap();
    let mut id = 0usize;
    let (items, _members, cov, proof) =
        engram_server::services::ask_engine::providers::exhaustive_callee_set(
            &g,
            None,
            "p",
            ts,
            &[EdgeKind::ApiCall, EdgeKind::SqlCalls, EdgeKind::Calls],
            &mut id,
        );
    let names: Vec<&str> = items.iter().filter_map(|i| i.title.as_deref()).collect();
    for want in ["apiGetX", "apiGetY", "apiGetZ"] {
        assert!(
            names.iter().any(|n| n.contains(want)),
            "the SAME-FILE trio must all survive — missing {want}: {names:?}"
        );
    }
    assert_eq!(cov.examined, 3, "every walked edge is counted: {cov:?}");
    // Round-4 P0-2: completeness is PROVEN, never asserted.
    assert!(proof.complete(), "a clean walk proves complete: {proof:?}");
    assert!(!cov.truncated);
}

fn c2_item(
    i: usize,
    provider: &str,
    path: &str,
) -> engram_server::services::ask_engine::evidence::EvidenceItem {
    use engram_server::services::ask_engine::evidence as ev;
    ev::EvidenceItem {
        evidence_id: format!("c2_{provider}_{i}"),
        kind: if provider == "callee_set" {
            ev::EvidenceKind::GraphRelation
        } else {
            ev::EvidenceKind::SourceCode
        },
        authority: ev::Authority::CurrentCode,
        path: Some(path.to_string()),
        lines: Some((i as u32 * 100 + 1, i as u32 * 100 + 40)),
        symbol_id: Some(format!("sym:{provider}:{i}")),
        title: None,
        content: format!("caller calls route_{i} — defined in {path}"),
        generation: None,
        commit: None,
        timestamp: None,
        confidence: 0.9,
        relevance: 0.9,
        extraction_method: "exhaustive_callee".into(),
        warnings: vec![],
        provider: provider.into(),
        score: None,
        directness: Some(0.9),
    }
}

#[test]
fn the_exhaustive_set_survives_the_per_path_cap() {
    // r70 live (doc-12 P0-2): 12 routes in ONE implementation file were
    // collapsed to 2 by the ranker's per-path anti-anchoring cap. Exhaustive
    // set items are facts, not anchoring bias — every one survives selection.
    use engram_server::services::ask_engine::ranking;
    let mut items: Vec<_> = (0..12)
        .map(|i| c2_item(i, "callee_set", "Site/api/api-impl.vb"))
        .collect();
    for i in 0..5 {
        items.push(c2_item(100 + i, "code", "Site/api/api-impl.vb"));
    }
    let out = ranking::rank_and_select_with_terms_exempt(items, 4, &[], Some("callee_set"));
    let set_kept = out.iter().filter(|e| e.provider == "callee_set").count();
    let code_kept = out.iter().filter(|e| e.provider == "code").count();
    assert_eq!(set_kept, 12, "every exhaustive-set item survives");
    assert!(
        code_kept <= 2,
        "non-set items still per-path capped, got {code_kept}"
    );
    assert!(out.len() <= 12 + 4, "cap applies to the non-exempt lane");
}

#[test]
fn an_api_question_walks_only_api_call_edges() {
    // r70 live: the set flooded every Calls callee (bootstrap callbacks,
    // jsonFill…) into "Which server API functions…". A kinds-restricted walk
    // returns the API routes and nothing else.
    use engram_core::RelPath;
    use engram_graph::{Edge, EdgeKind, GraphStore, Node};
    let dir = tempfile::tempdir().unwrap();
    let store = GraphStore::open(&dir.path().join("g.redb")).unwrap();
    let pid = "p_c2";
    let f = |name: &str, path: &str, id: &str| Node {
        node_id: id.to_string(),
        node_type: "function".to_string(),
        name: name.to_string(),
        namespace: String::new(),
        language: "typescript".to_string(),
        file_path: RelPath::new(path),
        start_line: 1,
        end_line: 9,
        generation: 1,
        metadata: None,
    };
    let nodes = vec![
        f(
            "panelInit",
            "ts/panel.ts",
            "sym:function:ts/panel.ts:panelInit:1",
        ),
        f(
            "api.routeA",
            "api/impl.vb",
            "sym:function:api/impl.vb:api.routeA:1",
        ),
        f(
            "helperFn",
            "ts/util.ts",
            "sym:function:ts/util.ts:helperFn:1",
        ),
    ];
    let edge = |kind: EdgeKind, target: &str| Edge {
        source_id: "sym:function:ts/panel.ts:panelInit:1".to_string(),
        target_id: target.to_string(),
        namespace: String::new(),
        language: String::new(),
        edge_kind: kind,
        weight: 1,
        generation: 1,
        metadata: None,
        updated_at_ms: 0,
    };
    let edges = vec![
        edge(EdgeKind::ApiCall, "sym:function:api/impl.vb:api.routeA:1"),
        edge(EdgeKind::Calls, "sym:function:ts/util.ts:helperFn:1"),
    ];
    store.upsert_nodes_and_edges(pid, &nodes, &edges).unwrap();
    let mut id = 0usize;
    let (items, _members, _cov, _proof) =
        engram_server::services::ask_engine::providers::exhaustive_callee_set(
            &store,
            None,
            pid,
            "ts/panel.ts",
            &[EdgeKind::ApiCall],
            &mut id,
        );
    assert_eq!(items.len(), 1, "only the ApiCall edge: {items:?}");
    assert!(items[0].content.contains("api.routeA"));
}

fn d_item(kind_def: bool) -> engram_server::services::ask_engine::evidence::EvidenceItem {
    use engram_server::services::ask_engine::evidence as ev;
    ev::EvidenceItem {
        evidence_id: "d1".to_string(),
        kind: if kind_def {
            ev::EvidenceKind::SourceCode
        } else {
            ev::EvidenceKind::GraphRelation
        },
        authority: ev::Authority::CurrentCode,
        path: Some("site/api/api-shared.vb".to_string()),
        lines: None,
        symbol_id: None,
        title: None,
        content: "orderPanel calls apiGetX — served by api-shared.vb".to_string(),
        generation: None,
        commit: None,
        timestamp: None,
        confidence: 0.9,
        relevance: 0.9,
        extraction_method: if kind_def {
            "definition".into()
        } else {
            "exhaustive_callee".into()
        },
        warnings: vec![],
        provider: if kind_def {
            "definition".into()
        } else {
            "callee_set".into()
        },
        score: Some(0.9),
        directness: Some(0.9),
    }
}

fn d_rep(
    provider: &str,
    count: usize,
    truncated: bool,
) -> engram_server::services::ask_engine::status::ProviderReport {
    engram_server::services::ask_engine::status::ProviderReport {
        provider: provider.to_string(),
        status: engram_server::services::ask_engine::status::ProviderStatus::Hit,
        count,
        note: None,
        examined: count,
        available: Some(count),
        truncated,
        proof: None,
    }
}

#[test]
fn an_exhaustive_callee_answer_needs_the_untruncated_set_arm() {
    // Doc-12 P0-1/P0-2: Answered must mean the traversal ran and completed.
    use engram_server::services::ask_engine::status::{
        AnswerStatus, FreshnessSnapshot, assess_status,
    };
    let plan = engram_server::services::ask_engine::planner::plan_query(
        "Which server API functions does orderPanel.ts call?",
    );
    // The evidence models the live shape: the definition arm cites the
    // asked .ts file (the Script modality gate rightly demands it), the
    // set item carries a route.
    let mut src = d_item(true);
    src.path = Some("site/ts/orderPanel.ts".to_string());
    let ev = vec![d_item(false), src];
    let snap = FreshnessSnapshot::default();
    // Without the callee_set arm: Partial, never Answered.
    let st = assess_status(&plan, &ev, &[d_rep("code", 5, true)], &snap, true);
    assert_eq!(st, AnswerStatus::Partial, "no traversal arm -> Partial");
    // With the set arm, untruncated: Answered.
    let st = assess_status(
        &plan,
        &ev,
        &[d_rep("code", 5, true), d_rep("callee_set", 16, false)],
        &snap,
        true,
    );
    assert_eq!(st, AnswerStatus::Answered, "complete traversal -> Answered");
    // A truncated set arm downgrades.
    let st = assess_status(&plan, &ev, &[d_rep("callee_set", 3, true)], &snap, true);
    assert_eq!(st, AnswerStatus::Partial, "truncated traversal -> Partial");
}

#[test]
fn a_completeness_file_set_without_a_traversal_is_partial() {
    // The camera lie (doc-12 P0-1): 1 of 2 files, status said Answered.
    use engram_server::services::ask_engine::status::{
        AnswerStatus, FreshnessSnapshot, assess_status,
    };
    let plan = engram_server::services::ask_engine::planner::plan_query(
        "Which TypeScript files under ts/map render the camera icon on a marker?",
    );
    let ev = vec![d_item(true)];
    let st = assess_status(
        &plan,
        &ev,
        &[d_rep("code", 5, false)],
        &FreshnessSnapshot::default(),
        true,
    );
    assert_eq!(
        st,
        AnswerStatus::Partial,
        "a completeness contract with no exhaustive traversal never claims Answered"
    );
}

#[test]
fn a_required_definition_facet_gates_answered() {
    use engram_server::services::ask_engine::status::{
        AnswerStatus, FreshnessSnapshot, assess_status,
    };
    let plan = engram_server::services::ask_engine::planner::plan_query(
        "Where is CanUserBulkUpdate defined?",
    );
    // Evidence with NO definition-arm item: Partial.
    let st = assess_status(
        &plan,
        &[d_item(false)],
        &[d_rep("code", 5, false)],
        &FreshnessSnapshot::default(),
        true,
    );
    assert_eq!(
        st,
        AnswerStatus::Partial,
        "missing Definition facet -> Partial"
    );
    // With a definition item: Answered.
    let st = assess_status(
        &plan,
        &[d_item(true)],
        &[d_rep("code", 5, false)],
        &FreshnessSnapshot::default(),
        true,
    );
    assert_eq!(
        st,
        AnswerStatus::Answered,
        "Definition facet satisfied -> Answered"
    );
}

fn s_store() -> (tempfile::TempDir, engram_graph::GraphStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = engram_graph::GraphStore::open(&dir.path().join("g.redb")).unwrap();
    (dir, store)
}

fn s_fn(name: &str, path: &str) -> engram_graph::Node {
    engram_graph::Node {
        node_id: format!("sym:function:{path}:{name}:1"),
        node_type: "function".to_string(),
        name: name.to_string(),
        namespace: String::new(),
        language: "typescript".to_string(),
        file_path: engram_core::RelPath::new(path),
        start_line: 1,
        end_line: 9,
        generation: 1,
        metadata: None,
    }
}

fn s_edge(src: &str, dst: &str) -> engram_graph::Edge {
    engram_graph::Edge {
        source_id: src.to_string(),
        target_id: dst.to_string(),
        namespace: String::new(),
        language: String::new(),
        edge_kind: engram_graph::EdgeKind::ApiCall,
        weight: 1,
        generation: 1,
        metadata: None,
        updated_at_ms: 0,
    }
}

#[test]
fn s1_dangling_target_is_counted_and_breaks_completeness() {
    // Round-4 P0-2: a dangling endpoint was silently skipped and the walk
    // still claimed truncated=false. Unknowns forbid completeness.
    use engram_graph::EdgeKind;
    let (_d, store) = s_store();
    let pid = "p_s1";
    let a = s_fn("panelInit", "ts/panel.ts");
    let aid = a.node_id.clone();
    store
        .upsert_nodes_and_edges(
            pid,
            &[a],
            &[s_edge(&aid, "sym:function:missing.vb:ghost:1")],
        )
        .unwrap();
    let mut id = 0usize;
    let (items, members, cov, proof) =
        engram_server::services::ask_engine::providers::exhaustive_callee_set(
            &store,
            None,
            pid,
            "ts/panel.ts",
            &[EdgeKind::ApiCall, EdgeKind::SqlCalls, EdgeKind::Calls],
            &mut id,
        );
    assert_eq!(
        proof.dangling_targets, 1,
        "the skip must be COUNTED: {proof:?}"
    );
    assert!(
        !proof.complete(),
        "an unknown endpoint forbids completeness"
    );
    assert!(
        cov.truncated,
        "ArmCoverage must stop lying about incompleteness"
    );
    assert!(items.is_empty() && members.is_empty());
}

#[test]
fn s1b_dangling_target_id_is_recorded_for_diagnosis() {
    // Counting a dangling target proves the walk is incomplete, but a caller
    // (or an operator diagnosing an ingestion defect) needs to know WHICH edge
    // is unverifiable — the id is recorded, not just tallied.
    use engram_graph::EdgeKind;
    let (_d, store) = s_store();
    let pid = "p_s1b";
    let a = s_fn("panelInit", "ts/panel.ts");
    let aid = a.node_id.clone();
    store
        .upsert_nodes_and_edges(
            pid,
            &[a],
            &[s_edge(&aid, "sym:function:missing.vb:ghost:1")],
        )
        .unwrap();
    let mut id = 0usize;
    let (_items, _members, _cov, proof) =
        engram_server::services::ask_engine::providers::exhaustive_callee_set(
            &store,
            None,
            pid,
            "ts/panel.ts",
            &[EdgeKind::ApiCall, EdgeKind::SqlCalls, EdgeKind::Calls],
            &mut id,
        );
    assert_eq!(
        proof.dangling_target_ids,
        vec!["sym:function:missing.vb:ghost:1".to_string()],
        "the specific unresolved target id must be recorded: {proof:?}"
    );
}

#[test]
fn s2_a_source_cap_is_detected_with_cap_plus_one() {
    // Round-4 P0-2: "fetches at most 500 ... returns truncated=false".
    // cap+1 discovery makes the cap OBSERVABLE.
    use engram_graph::EdgeKind;
    let (_d, store) = s_store();
    let pid = "p_s2";
    let mut nodes = Vec::new();
    for i in 0..3 {
        nodes.push(s_fn(&format!("fn{i}"), "ts/panel.ts"));
    }
    store.upsert_nodes_and_edges(pid, &nodes, &[]).unwrap();
    let mut id = 0usize;
    let (_items, _members, _cov, proof) =
        engram_server::services::ask_engine::providers::exhaustive_callee_set_with_caps(
            &store,
            None,
            pid,
            "ts/panel.ts",
            &[EdgeKind::ApiCall],
            2, // source cap
            500,
            &mut id,
        );
    assert!(
        proof.sources_discovered >= 3,
        "cap+1 must SEE past the cap: {proof:?}"
    );
    assert_eq!(proof.sources_processed, 2, "{proof:?}");
    assert!(proof.source_cap_hit, "{proof:?}");
    assert!(!proof.complete(), "a cap hit forbids completeness");
}

#[test]
fn s3_the_members_are_the_answer() {
    use engram_graph::EdgeKind;
    let (_d, store) = s_store();
    let pid = "p_s3";
    let a = s_fn("panelInit", "ts/panel.ts");
    let t1 = s_fn("api.routeA", "api/impl.vb");
    let t2 = s_fn("api.routeB", "api/impl2.vb");
    let aid = a.node_id.clone();
    let (i1, i2) = (t1.node_id.clone(), t2.node_id.clone());
    store
        .upsert_nodes_and_edges(pid, &[a, t1, t2], &[s_edge(&aid, &i1), s_edge(&aid, &i2)])
        .unwrap();
    let mut id = 0usize;
    let (_items, members, _cov, proof) =
        engram_server::services::ask_engine::providers::exhaustive_callee_set(
            &store,
            None,
            pid,
            "ts/panel.ts",
            &[EdgeKind::ApiCall],
            &mut id,
        );
    assert_eq!(members.len(), 2, "{members:?}");
    let m = members
        .iter()
        .find(|m| m.display_name == "api.routeA")
        .unwrap();
    assert_eq!(m.target_node_id, "sym:function:api/impl.vb:api.routeA:1");
    assert_eq!(m.relation, "api_call");
    assert_eq!(m.source_node_id.as_deref(), Some(aid.as_str()));
    assert_eq!(m.path.as_deref(), Some("api/impl.vb"));
    assert!(proof.complete());
}

#[test]
fn s4_the_report_json_carries_members_and_proof() {
    use engram_server::services::ask_engine::providers::{AnswerMember, CoverageProof};
    use engram_server::services::ask_engine::report::{AskReport, to_json};
    use engram_server::services::ask_engine::status::{
        AnswerStatus, FreshnessSnapshot, ProviderReport, ProviderStatus,
    };
    let r = AskReport {
        question: "q".into(),
        plan: engram_server::services::ask_engine::planner::plan_query("q"),
        status: AnswerStatus::Answered,
        mode: "retrieval_only".into(),
        evidence: vec![],
        conflicts: vec![],
        unknowns: vec![],
        next_best: vec![],
        snapshot: FreshnessSnapshot::default(),
        providers: vec![ProviderReport {
            provider: "callee_set".into(),
            status: ProviderStatus::Hit,
            count: 1,
            note: None,
            examined: 1,
            available: Some(1),
            truncated: false,
            proof: Some(CoverageProof::default()),
        }],
        answer_members: vec![AnswerMember {
            target_node_id: "t".into(),
            display_name: "iopGetProperties".into(),
            relation: "api_call".into(),
            source_node_id: None,
            path: None,
        }],
    };
    let j = serde_json::to_string(&to_json(&r)).unwrap();
    assert!(j.contains("answer_members"), "{j}");
    assert!(j.contains("iopGetProperties"), "{j}");
    assert!(j.contains("sources_discovered"), "{j}");
    let md = engram_server::services::ask_engine::report::render_markdown(&r);
    assert!(
        md.contains("iopGetProperties"),
        "members must be RENDERED, not JSON-only:
{md}"
    );
}

#[test]
fn s5_a_clean_small_walk_proves_complete() {
    // The honest positive that replaces "an exhaustive walk never
    // truncates": completeness is PROVEN by counters, not asserted.
    use engram_graph::EdgeKind;
    let (_d, store) = s_store();
    let pid = "p_s5";
    let a = s_fn("panelInit", "ts/panel.ts");
    let t = s_fn("api.routeA", "api/impl.vb");
    let (aid, tid) = (a.node_id.clone(), t.node_id.clone());
    store
        .upsert_nodes_and_edges(pid, &[a, t], &[s_edge(&aid, &tid)])
        .unwrap();
    let mut id = 0usize;
    let (_items, members, cov, proof) =
        engram_server::services::ask_engine::providers::exhaustive_callee_set(
            &store,
            None,
            pid,
            "ts/panel.ts",
            &[EdgeKind::ApiCall],
            &mut id,
        );
    assert!(proof.complete(), "{proof:?}");
    assert!(!cov.truncated);
    assert_eq!(proof.dangling_targets, 0);
    assert_eq!(members.len(), 1);
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
