use engram_core::RelPath;
use engram_index::{
    parsing::ExtractedSymbol,
    state_extractor::{analyze_state_affinity, extract_state_accesses_with_members},
};

fn member(name: &str, kind: &str, start: u32, end: u32) -> ExtractedSymbol {
    ExtractedSymbol {
        name: name.into(),
        kind: kind.into(),
        start_line: start,
        end_line: end,
        metadata: None,
    }
}

const SOURCE: &str = "Public Class Account\nPublic Function CheckAccess() As Boolean\nReturn True\nEnd Function\nPublic ReadOnly Property Theme As String\nGet\nReturn CStr(Session(\"Theme\"))\nEnd Get\nEnd Property\nPrivate cached As Object = Session(\"Initial\")\nEnd Class\n";

#[test]
fn property_state_is_not_owned_by_the_preceding_method() {
    let members = vec![member("Account.CheckAccess", "function", 2, 4)];
    let (_, edges) =
        extract_state_accesses_with_members(&RelPath::new("Account.vb"), SOURCE, "vbnet", &members);
    let theme = edges
        .iter()
        .find(|edge| edge.target_name == "state:Session:Theme")
        .unwrap();
    assert_eq!(
        theme.source_kind, "file",
        "Missing property ranges must not become false method dependencies"
    );
    assert_eq!(theme.source_name, "Account.vb");
    let (_, affinities) = analyze_state_affinity(&edges, &RelPath::new("Account.vb"));
    assert!(
        affinities.is_empty(),
        "Unowned accesses must not establish same-method co-access"
    );
}

#[test]
fn parsed_member_ranges_preserve_property_and_field_ownership() {
    let members = vec![
        member("Account.CheckAccess", "function", 2, 4),
        member("Account.Theme", "property", 5, 9),
        member("Account.cached", "field", 10, 10),
    ];
    let (_, edges) =
        extract_state_accesses_with_members(&RelPath::new("Account.vb"), SOURCE, "vbnet", &members);
    let theme = edges
        .iter()
        .find(|edge| edge.target_name == "state:Session:Theme")
        .unwrap();
    assert_eq!(
        (
            &*theme.source_name,
            &*theme.source_kind,
            theme.source_start_line
        ),
        ("Account.Theme", "property", 7)
    );
    let initial = edges
        .iter()
        .find(|edge| edge.target_name == "state:Session:Initial")
        .unwrap();
    assert_eq!(
        (
            &*initial.source_name,
            &*initial.source_kind,
            initial.source_start_line
        ),
        ("Account.cached", "field", 10)
    );
}

#[test]
fn nested_members_win_but_ambiguous_ranges_remain_file_scoped() {
    let source =
        "class Account {\nvoid Outer() {\nvoid Inner() {\nvar value = Session[\"Cart\"];\n}\n}\n}";
    let mut members = vec![
        member("Outer", "function", 2, 6),
        member("Inner", "function", 3, 5),
    ];
    let path = RelPath::new("Account.cs");
    let (_, edges) = extract_state_accesses_with_members(&path, source, "csharp", &members);
    assert_eq!(edges[0].source_name, "Inner");
    members.push(member("Other", "function", 3, 5));
    let (_, edges) = extract_state_accesses_with_members(&path, source, "csharp", &members);
    assert_eq!(edges[0].source_kind, "file");
}
