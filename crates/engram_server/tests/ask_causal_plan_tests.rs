#![allow(clippy::unwrap_used)]
//! External audit round 2, item 8 — the causal golden suite (live r43, 10/20):
//! the graph carried the TS→API route edges, but the planner never sent the
//! question to the arm that reads them. Three planner defects:
//!
//! * "What calls X" / "Which TypeScript calls X" / "What depends on X" /
//!   "Which frontend files depend on the X API" carried no Usage intent —
//!   only "who calls"/"who uses" did — so the symbol-references arm never ran.
//! * generic technology nouns ("API", "TypeScript", "VB") were taken as symbol
//!   mentions; "API" resolved to `ConfigSettings.Security.API` and its
//!   neighbours filled the evidence cap.
//! * an all-lowercase identifier ("getimg") was never a symbol mention, even
//!   when the question types it ("the getimg web method").
use engram_server::services::ask_engine::plan::{EntityKind, Intent};
use engram_server::services::ask_engine::planner::plan_query;

fn intents(q: &str) -> Vec<Intent> {
    plan_query(q).intents.iter().map(|(i, _)| *i).collect()
}

fn entities(q: &str) -> Vec<(String, EntityKind)> {
    plan_query(q)
        .entities
        .iter()
        .map(|e| (e.text.clone(), e.guessed_kind))
        .collect()
}

fn entity_texts(q: &str) -> Vec<String> {
    entities(q).into_iter().map(|(t, _)| t).collect()
}

#[test]
fn what_calls_x_is_a_usage_question() {
    let q = "What calls prGetSubProjects?";
    assert!(intents(q).contains(&Intent::Usage), "{:?}", intents(q));
    assert_eq!(entity_texts(q), vec!["prGetSubProjects"]);
}

#[test]
fn which_typescript_calls_x_asks_about_x_only() {
    let q = "Which TypeScript calls rvGetUsersForFilter?";
    assert!(intents(q).contains(&Intent::Usage), "{:?}", intents(q));
    assert_eq!(entity_texts(q), vec!["rvGetUsersForFilter"]);
}

#[test]
fn the_word_api_is_not_a_symbol_mention() {
    let q = "Who uses the athGetByFilter API?";
    assert!(intents(q).contains(&Intent::Usage), "{:?}", intents(q));
    assert_eq!(entity_texts(q), vec!["athGetByFilter"]);
}

#[test]
fn a_lowercase_name_typed_as_a_web_method_is_a_symbol_mention() {
    let q = "Which TypeScript calls the getimg web method, and which VB file implements it?";
    let ents = entities(q);
    assert!(
        ents.iter()
            .any(|(t, k)| t == "getimg" && *k == EntityKind::Symbol),
        "{ents:?}"
    );
    assert!(
        !ents
            .iter()
            .any(|(t, _)| t.eq_ignore_ascii_case("TypeScript") || t.eq_ignore_ascii_case("VB")),
        "{ents:?}"
    );
    assert!(intents(q).contains(&Intent::Usage), "{:?}", intents(q));
}

#[test]
fn what_depends_on_x_is_a_usage_question() {
    let q = "What depends on usGetAllUserAccessObjects?";
    assert!(intents(q).contains(&Intent::Usage), "{:?}", intents(q));
    assert_eq!(entity_texts(q), vec!["usGetAllUserAccessObjects"]);
}

#[test]
fn which_files_depend_on_the_x_api_is_a_usage_question_about_x() {
    let q = "Which frontend files depend on the permitDelete API?";
    assert!(intents(q).contains(&Intent::Usage), "{:?}", intents(q));
    assert_eq!(entity_texts(q), vec!["permitDelete"]);
}

#[test]
fn what_a_file_calls_is_not_a_usage_question() {
    let q = "Which server API functions does ioMarkerInfowindow.ts call?";
    let ents = entities(q);
    assert_eq!(
        ents,
        vec![("ioMarkerInfowindow.ts".to_string(), EntityKind::File)],
        "{ents:?}"
    );
    assert!(
        !intents(q).contains(&Intent::Usage),
        "callees, not callers: {:?}",
        intents(q)
    );
}

#[test]
fn a_generic_verb_typed_as_an_endpoint_is_not_a_symbol_mention() {
    // Live r44 (ox_history_2): "update endpoint" made `update` a symbol
    // mention, so a correct abstention became an answer without PR evidence.
    let q = "Which merged PR introduced the bulk base-type update endpoint?";
    assert!(
        !entity_texts(q)
            .iter()
            .any(|t| t.eq_ignore_ascii_case("update")),
        "{:?}",
        entities(q)
    );
    assert!(intents(q).contains(&Intent::History), "{:?}", intents(q));
}

#[test]
fn a_possessive_apostrophe_is_not_a_quote() {
    // Live r46: "map's ... marker's" minted the junk quoted mention
    // "s marker info window fetch a marker".
    let q = "How does the map's marker info window fetch a marker's images?";
    assert!(
        !entity_texts(q).iter().any(|t| t.contains(" fetch ")),
        "{:?}",
        entities(q)
    );
}
