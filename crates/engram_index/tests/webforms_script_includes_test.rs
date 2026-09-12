//! Static hosting dependencies must not rely on component/page name similarity.
use engram_core::RelPath;
use engram_index::webforms::extract_webforms;
use std::path::Path;

#[test]
fn shared_bundle_supports_mixed_template_hosts_and_skips_razor_expressions() {
    let root = Path::new("/project");
    let (_, webforms) = extract_webforms(
        root,
        &RelPath::new("Pages/Planning.aspx"),
        r#"<script src="/Scripts/shared.js"></script>"#,
    );
    assert!(webforms.iter().any(|e| e.kind == "includes_file"
        && e.source_kind == "page"
        && e.target_name == "Scripts/shared.js"));
    for host in [
        "Views/Review.cshtml",
        "Views/Review.vbhtml",
        "Views/Review.razor",
        "public/Review.html",
        "public/Review.htm",
    ] {
        let edges = engram_index::webforms::extract_template_script_includes(
            root,
            &RelPath::new(host),
            r#"
<script src="../Scripts/shared.js"></script>
<script src="@Url.Content("~/Scripts/dynamic.js")"></script>
<script src="/Scripts/@(bundle).js"></script>
@* <script src="/Scripts/commented.js"></script> *@
"#,
        );
        assert_eq!(edges.len(), 1, "{host}: {edges:?}");
        assert_eq!(edges[0].source_kind, "file");
        assert_eq!(edges[0].source_name, host);
        assert_eq!(edges[0].target_kind.as_deref(), Some("file"));
        assert_eq!(edges[0].target_name, "Scripts/shared.js");
    }
}

#[test]
fn shared_bundle_has_two_distinct_hosts() {
    let hosts = [
        ("Pages/Planning.aspx", "../Scripts/./shared.js?v=2#load"),
        ("Admin/Review.aspx", "~/Scripts/shared.js"),
        ("Other.aspx", "/Scripts/unrelated.js"),
    ];
    let mut consumers = Vec::new();
    for (host, src) in hosts {
        let markup = format!("<%@ Page %>\n<script src=\"{src}\"></script>");
        let (_, edges) = extract_webforms(Path::new("/project"), &RelPath::new(host), &markup);
        let edge = edges
            .iter()
            .find(|e| e.kind == "includes_file")
            .expect("script dependency");
        assert_eq!(edge.source_kind, "page");
        assert_eq!(edge.source_name, host);
        assert_eq!(edge.source_start_line, 1); // ExtractedEdge lines are zero-based.
        assert_eq!(edge.target_kind.as_deref(), Some("file"));
        if edge.target_name == "Scripts/shared.js" {
            consumers.push(edge.source_name.clone());
        }
    }
    assert_eq!(consumers, ["Pages/Planning.aspx", "Admin/Review.aspx"]);
}

#[test]
fn script_paths_normalize_and_deduplicate_without_guessing() {
    let markup = r#"
<SCRIPT defer SRC = '/Scripts/sub/../shared.js?x=1&amp;y=2#top'></SCRIPT>
<script src="~/Scripts/shared.js"></script>
<script src=../Scripts/shared.js></script>
<script src="..\Scripts\second.js"></script>
<script src="/elsewhere/shared.js"></script>
"#;
    let (_, edges) = extract_webforms(
        Path::new("/project"),
        &RelPath::new("Pages/Host.aspx"),
        markup,
    );
    let targets: Vec<_> = edges
        .iter()
        .filter(|e| e.kind == "includes_file")
        .map(|e| e.target_name.as_str())
        .collect();
    assert_eq!(
        targets,
        [
            "Scripts/shared.js",
            "Scripts/second.js",
            "elsewhere/shared.js"
        ]
    );
}

#[test]
fn remote_dynamic_commented_and_non_src_attributes_are_not_dependencies() {
    let markup = r##"
<script src="https://cdn.example/a.js"></script>
<script src="//cdn.example/a.js"></script>
<script src="data:text/javascript,foo"></script>
<script src="<%= ResolveUrl("~/Scripts/a.js") %>"></script>
<script src="{{ bundle }}"></script>
<script src="../../outside.js"></script>
<script src="/../outside.js"></script>
<script src="#fragment"></script>
<script data-src="/Scripts/a.js"></script>
<script title='src="/Scripts/a.js"'></script>
<!-- <script src="/Scripts/a.js"></script> -->
<%-- <script src="/Scripts/a.js"></script> --%>
<script>const example = '<script src="/Scripts/a.js">';</script>
<script runat="server" src="/Server/code.cs"></script>
"##;
    let (_, edges) = extract_webforms(
        Path::new("/project"),
        &RelPath::new("Pages/Host.aspx"),
        markup,
    );
    assert!(
        edges.iter().all(|e| e.kind != "includes_file"),
        "unexpected dependencies: {edges:?}"
    );
}

#[test]
fn static_script_paths_survive_dynamic_query_and_fragment_values() {
    let root = Path::new("/project");
    let rel = RelPath::new("Pages/Host.aspx");
    let (_, edges) = extract_webforms(
        root,
        &rel,
        r#"
<script src="../Scripts/shared.js?v=<%= Version %>"></script>
<script src="../Scripts/fragment.js#<%= Anchor %>"></script>
<script src="https://cdn.example/remote.js?v=<%= Version %>"></script>
<script src="../Scripts/<%= Bundle %>.js?v=1"></script>
"#,
    );
    let targets: Vec<_> = edges
        .iter()
        .filter(|e| e.kind == "includes_file")
        .map(|e| e.target_name.as_str())
        .collect();
    assert_eq!(targets, ["Scripts/shared.js", "Scripts/fragment.js"]);
    let edges = engram_index::webforms::extract_template_script_includes(
        root,
        &RelPath::new("Views/Review.cshtml"),
        r#"
<script src="../Scripts/shared.js?v=@Version"></script>
<script src="../Scripts/fragment.js#@(Anchor)"></script>
<script src="//cdn.example/remote.js?v=@Version"></script>
<script src="../Scripts/@(Bundle).js?v=1"></script>
"#,
    );
    let targets: Vec<_> = edges.iter().map(|e| e.target_name.as_str()).collect();
    assert_eq!(targets, ["Scripts/shared.js", "Scripts/fragment.js"]);
}

#[test]
fn script_dependencies_coexist_with_server_side_includes_and_inline_code() {
    let markup = r#"<!--#include file="../Shared/header.inc" -->
<script src="../Scripts/shared.js"></script>
<script>window.resources = { Save: 'Save' };</script>"#;
    let (_, edges) = extract_webforms(
        Path::new("/project"),
        &RelPath::new("Pages/Host.aspx"),
        markup,
    );
    let targets: Vec<_> = edges
        .iter()
        .filter(|e| e.kind == "includes_file")
        .map(|e| e.target_name.as_str())
        .collect();
    assert!(targets.contains(&"Shared/header.inc"));
    assert!(targets.contains(&"Scripts/shared.js"));
    assert_eq!(targets.len(), 2);
}
