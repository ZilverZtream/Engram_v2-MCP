#![allow(clippy::unwrap_used)]
//! Item 8, cycle 31 (live r47): the compound-name join was too eager.
//! "icon picker" (an incidental 2-word pair) seized bootstrap-iconpicker.css
//! and displaced ox_multi_2's real evidence; "marker info window" is honestly
//! ambiguous across five *MarkerInfowindow files, and the fallback 2-word join
//! "mapmarker" hijacked the mention MID-STRING onto
//! GisPdfElementFactoryForMapMarkers. The picker is a pure function with three
//! rules: three words or more, prefix/suffix containment, one distinct stem.
use engram_server::services::ask_engine::resolver::compound_join_pick;

#[test]
fn a_two_word_pair_never_picks_a_file() {
    // ox_multi_2: "… the marker editor's icon picker?"
    let words = vec![
        "custom", "marker", "icon", "from", "the", "upload", "control", "marker", "editor", "icon",
        "picker",
    ];
    let stems = vec![
        "bootstrap-iconpicker".to_string(),
        "ctrl_files".to_string(),
        "marker_edit".to_string(),
    ];
    assert_eq!(compound_join_pick(&words, &stems), None);
}

#[test]
fn an_ambiguous_family_and_a_mid_string_join_pick_nothing() {
    // ox_multi_4: five *MarkerInfowindow stems; "mapmarker" sits mid-string in
    // gispdfelementfactoryformapmarkers.
    let words = vec![
        "the", "map", "marker", "info", "window", "fetch", "marker", "images",
    ];
    let stems = vec![
        "atamarkerinfowindow".to_string(),
        "iomarkerinfowindow".to_string(),
        "permitmarkerinfowindow".to_string(),
        "plmarkerinfowindow".to_string(),
        "vehiclemarkerinfowindow".to_string(),
        "gispdfelementfactoryformapmarkers".to_string(),
    ];
    assert_eq!(compound_join_pick(&words, &stems), None);
}

#[test]
fn a_three_word_suffix_join_with_one_stem_wins() {
    let words = vec![
        "how", "does", "the", "order", "info", "panel", "fetch", "images",
    ];
    let stems = vec![
        "orderinfopanel".to_string(),
        "orderlines".to_string(),
        "api-images".to_string(),
    ];
    assert_eq!(compound_join_pick(&words, &stems), Some(0));
}

#[test]
fn a_twin_stem_counts_once() {
    // ts + compiled js share the stem — still one distinct stem.
    let words = vec!["the", "order", "info", "panel", "loads"];
    let stems = vec!["orderinfopanel".to_string(), "orderinfopanel".to_string()];
    assert!(matches!(compound_join_pick(&words, &stems), Some(0 | 1)));
}
