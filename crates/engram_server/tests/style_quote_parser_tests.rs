#![allow(clippy::unwrap_used)]
use engram_core::{Config, ProjectRecord};
use engram_index::parsing::js_quoted_strings;
use engram_server::{
    services::pre_commit_review_service::{
        ConventionCategory, GateStatus, ReviewConfig, ReviewFinding, extract_conventions,
        gates::StyleGate, run_pre_commit_review_with,
    },
    state::AppState,
};
use std::path::Path;
fn base(quote: char) -> String {
    (0..32)
        .map(|n| format!("const existing{n} = {quote}value{n}{quote};\n"))
        .collect()
}
fn quote_conventions(source: &str, file: &str) -> Vec<String> {
    extract_conventions(source, file)
        .into_iter()
        .filter(|c| c.category == ConventionCategory::StringQuotes)
        .map(|c| c.value)
        .collect()
}
async fn review(file: &str, source: &str, start: usize, added: &str) -> Vec<ReviewFinding> {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join(file), source).unwrap();
    let (state, _) = AppState::new(Config {
        data_dir: tmp.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })
    .unwrap();
    state
        .registry
        .put_project(&ProjectRecord {
            project_id: "quote-style".into(),
            project_name: "quote-style".into(),
            directory: root.to_string_lossy().into_owned(),
            project_type: "general".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            reindex_required_since_ms: None,
        })
        .unwrap();
    state
        .registry
        .set_meta("quote-style", "active_generation", "1")
        .unwrap();
    let body: String = added.lines().map(|line| format!("+{line}\n")).collect();
    let diff = format!(
        "diff --git a/{file} b/{file}\n--- a/{file}\n+++ b/{file}\n@@ -{start},0 +{start},{} @@\n{body}",
        added.lines().count()
    );
    let (findings, _, _, outcomes) = run_pre_commit_review_with(
        &state,
        "quote-style",
        &root,
        1,
        &diff,
        &ReviewConfig::default(),
        vec![Box::new(StyleGate)],
    )
    .await
    .unwrap();
    if js_quoted_strings(Path::new(file), source).is_none() {
        assert!(
            outcomes
                .iter()
                .any(|o| matches!(o.status, GateStatus::Degraded { .. })),
            "{outcomes:?}"
        );
    }
    findings
        .into_iter()
        .filter(|f| f.title.contains("String quote style mismatch"))
        .collect()
}
#[test]
fn parsed_inventory_ignores_comment_regex_and_template_text_but_keeps_expression_strings() {
    let source = r#"// don't count 'comment' or "comment"
/* block's 'quotes' */
const pattern = /['"\/]+/g;
const url = "https://host/path?text=don't";
const escaped = 'can\'t';
const text = `raw 'single' "double" ${'expression'}`;
"#;
    let strings = js_quoted_strings(Path::new("sample.ts"), source).unwrap();
    assert_eq!(
        strings.iter().map(|s| s.quote).collect::<Vec<_>>(),
        vec!['"', '\'', '\'']
    );
    assert_eq!(
        &source[strings[0].start_byte..strings[0].end_byte],
        "\"https://host/path?text=don't\""
    );
    assert_eq!(strings[2].start_line, 6);
}
#[test]
fn tsx_jsx_attributes_do_not_vote_but_javascript_expressions_do() {
    for extension in ["tsx", "jsx"] {
        let source = "const view = <div title='markup' data-label=\"markup\">don't count text<span>{'expression'}</span></div>;";
        let strings = js_quoted_strings(Path::new(&format!("view.{extension}")), source).unwrap();
        assert_eq!(strings.len(), 1, "{strings:?}");
        assert_eq!(
            &source[strings[0].start_byte..strings[0].end_byte],
            "'expression'"
        );
    }
    assert!(js_quoted_strings(Path::new("typed.ts"), "const typed = <string>\"value\";").is_some());
}
#[test]
fn comments_cannot_establish_or_change_quote_convention_and_bounds_decline_uncertainty() {
    let comments = "// 'x' 'y' 'z' 'q'\n".repeat(40);
    assert!(quote_conventions(&comments, "code.ts").is_empty());
    assert_eq!(
        quote_conventions(&(base('"') + &comments), "code.ts"),
        vec!["double"]
    );
    assert!(js_quoted_strings(Path::new("code.ts"), "const broken = \"unterminated").is_none());
    assert!(
        quote_conventions(&(base('"') + "const broken = \"unterminated"), "code.ts").is_empty()
    );
    assert!(js_quoted_strings(Path::new("code.ts"), &" ".repeat(8 * 1024 * 1024 + 1)).is_none());
}
#[tokio::test]
async fn added_comment_apostrophes_and_block_comments_started_before_hunk_are_not_strings() {
    for (prefix, added) in [
        (
            base('"'),
            "// the item's name\nconst valid = \"value\"; // don't rewrite this\n",
        ),
        (
            base('"') + "/* unchanged block start\n",
            "the item's name has 'quotes'\n*/\n",
        ),
    ] {
        let start = prefix.lines().count() + 1;
        assert!(
            review("code.ts", &(prefix + added), start, added)
                .await
                .is_empty()
        );
    }
}
#[tokio::test]
async fn genuine_opposite_literals_are_flagged_even_with_both_quote_kinds_on_line() {
    for (quote, added) in [
        (
            '"',
            "const newValue = 'single'; const other = \"double\";\n",
        ),
        (
            '\'',
            "const newValue = \"double\"; const other = 'single';\n",
        ),
    ] {
        let findings = review("code.ts", &(base(quote) + added), 33, added).await;
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].lines, vec![33]);
    }
}
#[tokio::test]
async fn regex_and_template_raw_text_do_not_trigger_but_substitution_string_does() {
    let quiet = r#"const pattern = /['"\/]+/;
const text = `don't turn 'raw' into "quotes"`;
"#;
    assert!(
        review("code.js", &(base('"') + quiet), 33, quiet)
            .await
            .is_empty()
    );
    let active = "const text = `raw ${'expression'}`;\n";
    assert_eq!(
        review("code.js", &(base('"') + active), 33, active)
            .await
            .len(),
        1
    );
}
#[tokio::test]
async fn unicode_multiline_literal_keeps_start_location_and_diff_mismatch_is_not_evidence() {
    let prefix = base('"') + "// Unicode: \u{00e9}\n";
    let added = "const value = 'first\\\nsecond';\n";
    let strings = js_quoted_strings(Path::new("code.ts"), &(prefix.clone() + added)).unwrap();
    assert_eq!(
        (
            strings.last().unwrap().start_line,
            strings.last().unwrap().end_line
        ),
        (34, 35)
    );
    let findings = review("code.ts", &(prefix + added), 34, added).await;
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].lines, vec![34]);
    assert!(
        review(
            "code.ts",
            &(base('"') + "const actual = 'value';\n"),
            33,
            "const different = 'value';\n"
        )
        .await
        .is_empty()
    );
}

#[tokio::test]
async fn malformed_inventory_degrades_quote_coverage_and_generated_exemption_remains() {
    let bad = "const broken = \"unterminated\n";
    assert!(
        review("code.ts", &(base('"') + bad), 33, bad)
            .await
            .is_empty()
    );
    let added = "const newValue = 'single';\n";
    assert!(
        review("code.generated.ts", &(base('"') + added), 33, added)
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn mjs_uses_the_same_parsed_quote_check_as_js() {
    let added = "const newValue = 'single';\n";
    let findings = review("module.mjs", &(base('"') + added), 33, added).await;
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!(findings[0].lines, vec![33]);
}
