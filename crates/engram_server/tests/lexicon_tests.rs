#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 row 1 (owner decision 10:58): the project's own
//! `.resx` pairs are a deterministic EN↔SV lexicon. Unit level: pairing,
//! filtering, longest-match translation, concept terms, cache signature.

use engram_server::services::lexicon::{
    LexiconHit, ascii_fold, build_lexicon, concept_terms, find_resx_pairs, resx_signature,
    translate,
};

fn resx(entries: &[(&str, &str)]) -> String {
    let mut s = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root>\n");
    for (k, v) in entries {
        s.push_str(&format!(
            "  <data name=\"{k}\" xml:space=\"preserve\">\n    <value>{v}</value>\n  </data>\n"
        ));
    }
    s.push_str("</root>\n");
    s
}

fn fixture() -> tempfile::TempDir {
    let tmp = tempfile::TempDir::new().unwrap();
    let res = tmp.path().join("Site/App_GlobalResources");
    std::fs::create_dir_all(&res).unwrap();
    std::fs::write(
        res.join("text.resx"),
        resx(&[
            ("Registration_of_quantities", "Mängdredovisning"),
            ("Registration_of_CAW", "ÄTA-registrering"),
            ("Fiber_installation_plan", "Fiberinstallationsplan"),
            ("Save", "Spara"),
            ("Back", "Tillbaka"),
            ("Dashboard", "Kontrollpanelen"),
            ("Count_fmt", "{0} st"),
            ("Same", "Status"),
        ]),
    )
    .unwrap();
    std::fs::write(
        res.join("text.en.resx"),
        resx(&[
            ("Registration_of_quantities", "Reporting of Quantities"),
            ("Registration_of_CAW", "Change Requests"),
            ("Fiber_installation_plan", "Fiber installation plan"),
            ("Save", "Save"),
            ("Back", "Back"),
            ("Dashboard", "Dashboard"),
            ("Count_fmt", "{0} pcs"),
            ("Same", "Status"),
        ]),
    )
    .unwrap();
    // A German culture file must not pair with anything.
    std::fs::write(res.join("text.de.resx"), resx(&[("Save", "Speichern")])).unwrap();
    tmp
}

#[test]
fn the_default_culture_resx_pairs_with_its_english_sibling() {
    let tmp = fixture();
    let pairs = find_resx_pairs(tmp.path());
    assert_eq!(pairs.len(), 1, "{pairs:?}");
    assert!(pairs[0].0.ends_with("text.resx") && pairs[0].1.ends_with("text.en.resx"));
}

#[test]
fn the_lexicon_keeps_domain_phrases_and_drops_generic_words_and_templates() {
    let tmp = fixture();
    let lex = build_lexicon(tmp.path());
    let en: Vec<&str> = lex.pairs.iter().map(|(e, _)| e.as_str()).collect();
    assert!(en.contains(&"reporting of quantities"), "{en:?}");
    assert!(en.contains(&"change requests"), "{en:?}");
    assert!(en.contains(&"fiber installation plan"), "{en:?}");
    assert!(
        en.contains(&"dashboard"),
        "one long word can be an entity: {en:?}"
    );
    assert!(
        !en.contains(&"save") && !en.contains(&"back"),
        "generic short words are not entities: {en:?}"
    );
    assert!(
        !en.iter().any(|e| e.contains("pcs")),
        "templates with placeholders are dropped: {en:?}"
    );
    assert!(
        !en.contains(&"status"),
        "identical values carry no translation: {en:?}"
    );
    assert_eq!(lex.resx_files, 3);
}

#[test]
fn translation_is_longest_match_over_the_story() {
    let tmp = fixture();
    let lex = build_lexicon(tmp.path());
    let story = "As a project manager I want the Reporting of Quantities to show the change requests per fiber installation plan, and a Save button";
    let hits = translate(story, &lex);
    let sv: Vec<&str> = hits.iter().map(|h| h.sv.as_str()).collect();
    assert_eq!(
        sv,
        vec![
            "Mängdredovisning",
            "ÄTA-registrering",
            "Fiberinstallationsplan"
        ],
        "{hits:?}"
    );
    let terms = concept_terms(&hits);
    assert!(
        terms.contains(&"mängdredovisning".to_string())
            && terms.contains(&"mangdredovisning".to_string()),
        "{terms:?}"
    );
    assert!(
        terms.contains(&"fiberinstallationsplan".to_string()),
        "{terms:?}"
    );
    assert!(
        terms.contains(&"registrering".to_string()),
        "each Swedish token >= 5 letters counts: {terms:?}"
    );
    assert!(!terms.iter().any(|t| t == "spara"), "{terms:?}");
    assert_eq!(ascii_fold("Mängdredovisning"), "Mangdredovisning");
    assert!(translate("nothing the lexicon knows", &lex).is_empty());
    let _ = LexiconHit {
        en: "x".into(),
        sv: "y".into(),
    };
}

#[test]
fn the_signature_changes_when_a_resource_file_changes() {
    let tmp = fixture();
    let a = resx_signature(tmp.path());
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(
        tmp.path().join("Site/App_GlobalResources/text.en.resx"),
        resx(&[
            ("Registration_of_quantities", "Reporting of Quantities"),
            ("New_key", "Inspection round"),
        ]),
    )
    .unwrap();
    assert_ne!(a, resx_signature(tmp.path()));
}
