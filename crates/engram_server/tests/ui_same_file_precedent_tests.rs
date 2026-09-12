#![allow(clippy::unwrap_used)]
use engram_core::config::Config;
use engram_server::services::pre_commit_review_service::gates::UiHouseStyleGate;
use engram_server::services::pre_commit_review_service::{Gate, GateContext, parse_unified_diff};
use engram_server::state::AppState;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

fn review(before: &str, added: &str, disk_override: Option<&str>) -> (Vec<String>, Vec<String>) {
    review_with_sibling(
        before,
        added,
        disk_override,
        "<div class=\"ordinary\"></div>\n",
    )
}
fn review_with_sibling(
    before: &str,
    added: &str,
    disk_override: Option<&str>,
    sibling: &str,
) -> (Vec<String>, Vec<String>) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let source = format!("{before}{added}");
    std::fs::write(root.join("edit.master"), disk_override.unwrap_or(&source)).unwrap();
    std::fs::write(root.join("sibling.aspx"), sibling).unwrap();
    let state = AppState::new(Config {
        data_dir: temp.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })
    .unwrap()
    .0;
    let body = before
        .lines()
        .map(|l| format!(" {l}\n"))
        .chain(added.lines().map(|l| format!("+{l}\n")))
        .collect::<String>();
    let diff = format!(
        "diff --git a/edit.master b/edit.master\n--- a/edit.master\n+++ b/edit.master\n@@ -1,{} +1,{} @@\n{body}",
        before.lines().count(),
        source.lines().count()
    );
    let files = parse_unified_diff(&diff);
    let paths: HashSet<_> = files.iter().map(|f| f.path.clone()).collect();
    let ctx = GateContext {
        search_index_note: None,
        state: &state,
        graph: state.graph.clone(),
        registry: state.registry.clone(),
        project_id: "fixture",
        project_dir: &root,
        generation: 1,
        diff_files: &files,
        changed_paths: &paths,
        total_commits: 0,
        repo_rules: Arc::new(vec![]),
        files_by_parent: Arc::new(HashMap::new()),
        audit_function: None,
        degraded: Mutex::new(vec![]),
        caps: Mutex::new(vec![]),
    };
    let findings = UiHouseStyleGate
        .run(&ctx)
        .unwrap()
        .into_iter()
        .filter(|f| f.title.contains("class(es)"))
        .map(|f| format!("{} {}", f.title, f.detail))
        .collect();
    let degraded = ctx.degraded.into_inner().unwrap();
    (findings, degraded)
}
#[test]
fn unchanged_same_file_static_attribute_is_precedent() {
    for before in [
        "<input class=\"compact\" />\n",
        "<asp:TextBox\n CssClass='compact'\n />\n",
    ] {
        assert!(
            review(before, "<input class=\"compact\" />\n", None)
                .0
                .is_empty()
        );
    }
}
#[test]
fn new_only_attribute_never_self_validates_and_sample_is_explicit() {
    let (f, _) = review("<div></div>\n", "<input class=\"compact\" />\n", None);
    assert_eq!(f.len(), 1);
    assert!(f[0].contains("compact"));
    assert!(f[0].contains("1 sampled"));
    assert!(f[0].contains("12"));
}
#[test]
fn case_distinct_tokens_remain_distinct() {
    let (f, _) = review(
        "<input class=\"Compact\" />\n",
        "<input class=\"compact\" />\n",
        None,
    );
    assert_eq!(f.len(), 1);
    assert!(f[0].contains("`compact`"));
}
#[test]
fn comments_raw_text_and_fake_attributes_are_not_precedents() {
    for before in [
        "<!-- <input class=\"compact\" /> -->\n",
        "<%-- <input class=\"compact\" /> --%>\n",
        "<script>var x = '<input class=\"compact\" />';</script>\n",
        "<style>.x:after {content:'<input class=\"compact\" />';}</style>\n",
        "<textarea><input class=\"compact\" /></textarea>\n",
        "<div data-class=\"compact\"></div>\n",
        "<div title='class=\"compact\"'></div>\n",
        "<div class=\"<%= value %> compact\"></div>\n",
    ] {
        assert_eq!(
            review(before, "<input class=\"compact\" />\n", None)
                .0
                .len(),
            1,
            "{before}"
        );
    }
}
#[test]
fn added_comments_and_script_strings_are_not_class_advice() {
    for added in [
        "<!-- <input class=\"novel\" /> -->\n",
        "<script>var x = '<input class=\"novel\" />';</script>\n",
        "<div data-class=\"novel\"></div>\n",
        "<div title='class=\"novel\"'></div>\n",
    ] {
        assert!(review("<div></div>\n", added, None).0.is_empty(), "{added}");
    }
}
#[test]
fn multiline_attribute_touching_addition_is_not_precedent() {
    let (f, _) = review(
        "<input class=\"\n",
        "compact\" />\n<input class=\"compact\" />\n",
        None,
    );
    assert_eq!(f.len(), 1);
}
#[test]
fn stale_new_side_context_disables_class_inference() {
    let (f, d) = review(
        "<input class=\"compact\" />\n",
        "<input class=\"novel\" />\n",
        Some("<input class=\"different\" />\n<input class=\"novel\" />\n"),
    );
    assert!(f.is_empty());
    assert!(!d.is_empty());
}
#[test]
fn malformed_attribute_ownership_disables_class_inference() {
    let (f, d) = review(
        "<div title=\"unfinished\n",
        "<input class=\"novel\" />\n",
        None,
    );
    assert!(f.is_empty());
    assert!(!d.is_empty());
}

#[test]
fn attribute_inventory_is_case_sensitive_bounded_and_ownership_aware() {
    use engram_server::services::house_style::{markup_idioms, static_class_occurrences};
    let source = "<input disabled class='Compact wide' />\n<asp:Panel\n CssClass=\"small\"\n />\n";
    let occurrences = static_class_occurrences(source).unwrap();
    assert_eq!(occurrences.len(), 2);
    assert_eq!(occurrences[1].attribute_lines, 3..=3);
    assert_eq!(occurrences[1].tag_lines, 2..=4);
    let classes = markup_idioms(source).classes;
    assert!(classes.contains("Compact") && !classes.contains("compact"));
    for uncertain in [
        "<div class=\"safe\" CLASS=\"other\">",
        "<div class=\"unterminated",
        "<script>broken",
        "<!-- broken",
    ] {
        assert!(static_class_occurrences(uncertain).is_err(), "{uncertain}");
    }
    for dynamic in [
        "<div class=\"safe <%= Call(\"x\") %>\">",
        "<div class=\"safe &amp;\">",
        "<div class=\"safe {{value}}\">",
        "<div class=unquoted>",
    ] {
        assert!(markup_idioms(dynamic).classes.is_empty(), "{dynamic}");
    }
    assert!(static_class_occurrences(&" ".repeat(2 * 1024 * 1024 + 1)).is_err());
}

#[test]
fn malformed_sibling_is_unknown_not_class_absence() {
    let (f, d) = review_with_sibling(
        "<div></div>\n",
        "<input class=\"novel\" />\n",
        None,
        "<div class=\"broken",
    );
    assert!(f.is_empty());
    assert!(
        d.iter()
            .any(|s| s.contains("sibling class inventory unavailable"))
    );
}
#[test]
fn sibling_case_and_raw_script_self_close_are_not_guessed() {
    let (f, _) = review_with_sibling(
        "<div></div>\n",
        "<input class=\"compact\" />\n",
        None,
        "<div class=\"Compact\"></div>",
    );
    assert_eq!(f.len(), 1);
    let (f, _) = review_with_sibling(
        "<div></div>\n",
        "<input class=\"Compact\" />\n",
        None,
        "<div class=\"Compact\"></div>",
    );
    assert!(f.is_empty());
    assert!(
        review(
            "<div></div>\n",
            "<script/>var x = '<input class=\"novel\" />';</script>\n",
            None
        )
        .0
        .is_empty()
    );
}

#[test]
fn page_context_does_not_convert_unknown_classes_to_missing_advice() {
    use engram_server::services::house_style::house_style_for;
    let temp = tempfile::tempdir().unwrap();
    let sibling = "<div class=\"shared\"><uc:Widget /><asp:Label Text=\"<%$ Resources:text, Key %>\" /></div>";
    std::fs::write(temp.path().join("sibling.aspx"), sibling).unwrap();
    let unknown = house_style_for(temp.path(), "edit.aspx", "<div class=\"unterminated");
    assert!(!unknown.missing_in_page.iter().any(|s| s == "shared"));
    assert!(
        unknown.note.contains("current page unknown")
            && unknown.note.contains("Missing-class advice skipped")
    );
    assert!(unknown.missing_in_page.iter().any(|s| s == "uc:widget"));
    assert!(
        unknown
            .missing_in_page
            .iter()
            .any(|s| s == "Resources.text")
    );
    let known = house_style_for(temp.path(), "edit.aspx", "<div></div>");
    assert!(known.missing_in_page.iter().any(|s| s == "shared"));
    std::fs::write(
        temp.path().join("malformed.aspx"),
        "<div class=\"unterminated",
    )
    .unwrap();
    let partial = house_style_for(temp.path(), "edit.aspx", "<div></div>");
    assert!(!partial.missing_in_page.iter().any(|s| s == "shared"));
    assert!(
        partial
            .note
            .contains("1 of 2 sampled sibling inventories unknown")
    );
    assert!(partial.note.contains("Missing-class advice skipped"));
    assert!(
        partial
            .common_classes
            .iter()
            .any(|c| c.name == "shared" && c.siblings == 1)
    );
}

#[test]
fn tag_level_server_attributes_exclude_whole_tag_not_unrelated_static_markup() {
    use engram_server::services::house_style::static_class_occurrences;
    let dynamic = "<li class='before' <%# Choose(\"class='fake'\") %> CssClass='after'></li>\n";
    let source = format!("{dynamic}<input class='kept' />\n");
    let rows = static_class_occurrences(&source).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0]
            .classes
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["kept"]
    );
    assert_eq!(rows[0].attribute_lines, 2..=2);
    assert!(static_class_occurrences("<li <%# Unfinished()").is_err());
    assert!(static_class_occurrences("</li <%# Unexpected() %>>").is_err());
    let before = format!("{dynamic}<input class='compact' />\n");
    assert!(
        review(&before, "<input class='compact' />\n", None)
            .0
            .is_empty()
    );
    let (findings, degraded) = review(dynamic, "<input class='before' />\n", None);
    assert_eq!(findings.len(), 1);
    assert!(degraded.is_empty());
}
