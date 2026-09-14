//! ASP.NET Optimization bundle extraction.
//!
//! Bundle declarations and render calls live in different artifacts.  Preserve
//! that relationship through a stable virtual node:
//!
//! `markup file -> bundle:~/bundles/app -> Scripts/app.js`

use crate::parsing::ExtractedEdge;
use engram_core::RelPath;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;

static DIRECT_BUNDLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?is)\b(?P<kind>ScriptBundle|StyleBundle)\s*\(\s*(?:\"(?P<double>[^\"]+)\"|'(?P<single>[^']+)')\s*\)\s*\.\s*Include\s*\("#,
    )
    .expect("valid ASP.NET bundle declaration regex")
});

static BUNDLE_RENDER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?P<kind>Scripts|Styles)\s*\.\s*Render\s*\(")
        .expect("valid ASP.NET bundle render regex")
});

/// Extract direct `ScriptBundle` / `StyleBundle` declarations from VB or C#.
///
/// This deliberately accepts only statically named bundles and concrete member
/// paths. Dynamic expressions and wildcard members remain visible in source
/// search, but do not become misleading graph edges.
pub fn extract_bundle_definitions(rel_path: &RelPath, source: &str) -> Vec<ExtractedEdge> {
    let mut edges = Vec::new();
    let mut seen = HashSet::new();
    let language = source_language(rel_path);

    for declaration in DIRECT_BUNDLE.captures_iter(source) {
        let Some(whole) = declaration.get(0) else {
            continue;
        };
        let Some(raw_virtual_path) = declaration
            .name("double")
            .or_else(|| declaration.name("single"))
            .map(|m| m.as_str())
        else {
            continue;
        };
        let Some(bundle_id) = canonical_bundle_id(raw_virtual_path) else {
            continue;
        };
        let asset_kind = if declaration
            .name("kind")
            .is_some_and(|m| m.as_str().eq_ignore_ascii_case("StyleBundle"))
        {
            "style"
        } else {
            "script"
        };
        let line = line_at(source, whole.start());

        if seen.insert((
            rel_path.as_str().to_string(),
            bundle_id.clone(),
            "definition",
        )) {
            edges.push(ExtractedEdge {
                source_name: rel_path.as_str().to_string(),
                source_kind: "file".to_string(),
                source_start_line: line,
                source_language: language.clone(),
                target_name: bundle_id.clone(),
                target_kind: Some("asset_bundle".to_string()),
                target_start_line: Some(line),
                kind: "includes_file".to_string(),
                metadata: Some(HashMap::from([
                    ("include_type".to_string(), "bundle_definition".to_string()),
                    ("asset_kind".to_string(), asset_kind.to_string()),
                    ("virtual_path".to_string(), raw_virtual_path.to_string()),
                ])),
            });
        }

        // The regex ends immediately after Include's opening parenthesis.
        for (raw_member, member_offset) in quoted_arguments(source, whole.end() - 1) {
            let Some(target) = resolve_member_path(rel_path, &raw_member) else {
                continue;
            };
            if !seen.insert((bundle_id.clone(), target.clone(), "member")) {
                continue;
            }
            edges.push(ExtractedEdge {
                source_name: bundle_id.clone(),
                source_kind: "asset_bundle".to_string(),
                source_start_line: line_at(source, member_offset),
                source_language: language.clone(),
                target_name: target,
                target_kind: Some("file".to_string()),
                target_start_line: None,
                kind: "includes_file".to_string(),
                metadata: Some(HashMap::from([
                    ("include_type".to_string(), "bundle_member".to_string()),
                    ("asset_kind".to_string(), asset_kind.to_string()),
                    ("virtual_path".to_string(), raw_virtual_path.to_string()),
                    ("raw_path".to_string(), raw_member),
                ])),
            });
        }
    }
    edges
}

/// Extract `Scripts.Render(...)` and `Styles.Render(...)` calls from WebForms
/// and Razor-family markup.
pub fn extract_bundle_renders(
    rel_path: &RelPath,
    source: &str,
    source_kind: &str,
) -> Vec<ExtractedEdge> {
    let mut edges = Vec::new();
    let mut seen = HashSet::new();
    let language = source_language(rel_path);

    for render in BUNDLE_RENDER.captures_iter(source) {
        let Some(whole) = render.get(0) else {
            continue;
        };
        let asset_kind = if render
            .name("kind")
            .is_some_and(|m| m.as_str().eq_ignore_ascii_case("Styles"))
        {
            "style"
        } else {
            "script"
        };
        for (raw_virtual_path, offset) in quoted_arguments(source, whole.end() - 1) {
            let Some(bundle_id) = canonical_bundle_id(&raw_virtual_path) else {
                continue;
            };
            if !seen.insert(bundle_id.clone()) {
                continue;
            }
            edges.push(ExtractedEdge {
                source_name: rel_path.as_str().to_string(),
                source_kind: source_kind.to_string(),
                source_start_line: line_at(source, offset),
                source_language: language.clone(),
                target_name: bundle_id,
                target_kind: Some("asset_bundle".to_string()),
                target_start_line: None,
                kind: "includes_file".to_string(),
                metadata: Some(HashMap::from([
                    ("include_type".to_string(), "bundle_render".to_string()),
                    ("asset_kind".to_string(), asset_kind.to_string()),
                    ("virtual_path".to_string(), raw_virtual_path),
                ])),
            });
        }
    }
    edges
}

fn canonical_bundle_id(raw: &str) -> Option<String> {
    let mut path = raw.trim().replace('\\', "/");
    if path.is_empty()
        || path.contains(['<', '>', '{', '}', '@', ':', '&', '%', '*'])
        || path.starts_with("//")
    {
        return None;
    }
    if let Some(rest) = path.strip_prefix('/') {
        path = format!("~/{rest}");
    } else if !path.starts_with("~/") {
        path = format!("~/{}", path.trim_start_matches("./"));
    }
    let normalized = normalize_relative(path.trim_start_matches("~/"))?;
    Some(format!("bundle:~/{}", normalized.to_ascii_lowercase()))
}

fn resolve_member_path(rel_path: &RelPath, raw: &str) -> Option<String> {
    let path = raw
        .trim()
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .replace('\\', "/");
    if path.is_empty()
        || path.contains(['<', '>', '{', '}', '@', ':', '&', '%', '*'])
        || path.starts_with("//")
    {
        return None;
    }
    let scoped = if let Some(rooted) = path.strip_prefix("~/").or_else(|| path.strip_prefix('/')) {
        rooted.to_string()
    } else {
        let parent = rel_path.as_str().rsplit_once('/').map_or("", |(p, _)| p);
        format!("{parent}/{path}")
    };
    normalize_relative(&scoped)
}

fn normalize_relative(raw: &str) -> Option<String> {
    let mut parts = Vec::new();
    let slash_path = raw.replace('\\', "/");
    for part in slash_path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            value if value.contains('\0') => return None,
            value => parts.push(value),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Return statically quoted arguments inside the parenthesis at `open`.
/// Nested constructor calls are allowed; escaped C# and doubled VB quotes are
/// consumed without terminating the string.
fn quoted_arguments(source: &str, open: usize) -> Vec<(String, usize)> {
    let bytes = source.as_bytes();
    if bytes.get(open) != Some(&b'(') {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut depth = 1usize;
    let mut i = open + 1;
    while i < bytes.len() && depth > 0 {
        match bytes[i] {
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth -= 1;
                i += 1;
            }
            quote @ (b'\'' | b'"') => {
                let start = i;
                i += 1;
                let mut value = Vec::new();
                while i < bytes.len() {
                    if bytes[i] == quote {
                        // VB escapes quotes by doubling them.
                        if bytes.get(i + 1) == Some(&quote) {
                            value.push(quote);
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    // C# and Razor use backslash escaping in ordinary strings.
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        value.push(bytes[i + 1]);
                        i += 2;
                    } else {
                        value.push(bytes[i]);
                        i += 1;
                    }
                }
                if let Ok(value) = String::from_utf8(value) {
                    out.push((value, start));
                }
            }
            _ => i += 1,
        }
    }
    out
}

fn source_language(rel_path: &RelPath) -> String {
    Path::new(rel_path.as_str())
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("text")
        .to_ascii_lowercase()
}

fn line_at(source: &str, offset: usize) -> u32 {
    source[..offset.min(source.len())]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count() as u32
        + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_multiline_vb_bundle_members_to_a_canonical_virtual_node() {
        let source = r#"Public Shared Sub RegisterBundles(bundles As BundleCollection)
    bundles.Add(New ScriptBundle("~/Bundles/App").Include("~/scripts/a.js",
                                                           "~/scripts/b.js"))
End Sub"#;
        let edges = extract_bundle_definitions(&RelPath::new("App_Start/BundleConfig.vb"), source);
        assert_eq!(edges.len(), 3);
        assert_eq!(edges[0].target_name, "bundle:~/bundles/app");
        assert_eq!(edges[1].source_name, "bundle:~/bundles/app");
        assert_eq!(edges[1].target_name, "scripts/a.js");
        assert_eq!(edges[2].target_name, "scripts/b.js");
        assert_eq!(edges[1].source_start_line, 2);
        assert_eq!(edges[2].source_start_line, 3);
    }

    #[test]
    fn extracts_csharp_style_bundle_and_rejects_dynamic_or_wildcard_members() {
        let source = r#"bundles.Add(new StyleBundle("/Styles/Main").Include(
            "../content/site.css", "~/content/*.css", GetTheme()));"#;
        let edges = extract_bundle_definitions(&RelPath::new("Config/Bundles.cs"), source);
        assert_eq!(edges.len(), 2);
        assert_eq!(edges[0].target_name, "bundle:~/styles/main");
        assert_eq!(edges[1].target_name, "content/site.css");
    }

    #[test]
    fn extracts_webforms_and_razor_render_calls_with_the_same_identity() {
        let webforms = r#"<%= System.Web.Optimization.Scripts.Render("~/Bundles/App") %>"#;
        let razor = r#"@Scripts.Render("/bundles/app")"#;
        let a = extract_bundle_renders(&RelPath::new("Site.Master"), webforms, "page");
        let b = extract_bundle_renders(&RelPath::new("Views/_Layout.cshtml"), razor, "file");
        assert_eq!(a[0].target_name, "bundle:~/bundles/app");
        assert_eq!(b[0].target_name, a[0].target_name);
        assert_eq!(a[0].source_start_line, 1);
    }

    #[test]
    fn render_supports_multiple_static_bundles_but_ignores_expressions() {
        let source = r#"@Scripts.Render("~/bundles/core", themeBundle, "~/bundles/page")"#;
        let edges = extract_bundle_renders(&RelPath::new("page.vbhtml"), source, "file");
        assert_eq!(
            edges
                .iter()
                .map(|edge| edge.target_name.as_str())
                .collect::<Vec<_>>(),
            vec!["bundle:~/bundles/core", "bundle:~/bundles/page"]
        );
    }
}
