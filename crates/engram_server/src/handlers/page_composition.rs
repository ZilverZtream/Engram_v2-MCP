//! Bounded, source-backed WebForms composition evidence.
use regex::Regex;
use serde::Serialize;
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    io::Read,
    path::{Path, PathBuf},
    sync::LazyLock,
};

#[derive(Debug, Clone, Serialize, Default)]
pub struct Composition {
    pub files: Vec<Component>,
    pub bindings: Vec<Binding>,
    pub warnings: Vec<String>,
    pub runtime_verification: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct Component {
    pub path: String,
    pub kind: String,
    pub declared_by: String,
    pub source_hash: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Binding {
    pub child: String,
    pub master: String,
    pub placeholder: String,
    pub status: &'static str,
}

static DIRECTIVES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<%@\s*(\w+)\b(.*?)%>").expect("directives"));
static ATTRIBUTES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\b(\w+)\s*=\s*(?:"([^"]*)"|'([^']*)')"#).expect("attributes")
});
static COMMENTS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<%--.*?--%>").expect("server comments"));
static SERVER_CODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<%[^@].*?%>").expect("server code blocks"));
static CONTENT_TAGS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<asp:(ContentPlaceHolder|Content)\b([^>]*)>").expect("content tags")
});

fn attributes(text: &str) -> BTreeMap<String, String> {
    ATTRIBUTES
        .captures_iter(text)
        .filter_map(|cap| {
            Some((
                cap[1].to_ascii_lowercase(),
                cap.get(2).or_else(|| cap.get(3))?.as_str().into(),
            ))
        })
        .collect()
}

fn resolve(root: &Path, app: &Path, parent: &Path, value: &str) -> Result<PathBuf, String> {
    let value = value.replace('\\', "/");
    let path = if let Some(relative) = value.strip_prefix("~/") {
        app.join(relative)
    } else {
        if Path::new(&value).is_absolute() || value.contains(':') {
            return Err("absolute or external directive path".into());
        }
        parent.join(value)
    };
    let canonical = path.canonicalize().map_err(|error| error.to_string())?;
    if !canonical.starts_with(root) {
        return Err("directive target escapes the registered project".into());
    }
    Ok(canonical)
}

pub(super) fn collect(
    root: &Path,
    app: &Path,
    page: &Path,
    include_master: bool,
    include_codebehind: bool,
) -> Composition {
    let mut report = Composition {
        runtime_verification: "not_run; static directive declarations only",
        ..Default::default()
    };
    let Ok(root) = root.canonicalize() else {
        report.warnings.push("project root unavailable".into());
        return report;
    };
    let app = app.canonicalize().unwrap_or(root.clone());
    let Ok(page) = page.canonicalize() else {
        report.warnings.push("page unavailable".into());
        return report;
    };
    let mut queue = VecDeque::from([(page, "page".to_string(), String::new(), 0usize)]);
    let mut seen = HashSet::new();
    let mut contents: HashMap<String, (HashSet<String>, Vec<String>)> = HashMap::new();
    let mut masters = Vec::new();
    let mut total_bytes = 0usize;
    let mut directives_seen = 0usize;
    while let Some((path, kind, parent, depth)) = queue.pop_front() {
        if !path.starts_with(&root) {
            report
                .warnings
                .push("component escapes project root".into());
            continue;
        }
        if !seen.insert(path.clone()) {
            continue;
        }
        if depth > 8 || report.files.len() >= 32 {
            report
                .warnings
                .push("composition truncated (depth 8, files 32)".into());
            continue;
        }
        let relative = path
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let loaded = (|| -> std::io::Result<String> {
            let mut text = String::new();
            std::fs::File::open(&path)?
                .take(1024 * 1024 + 1)
                .read_to_string(&mut text)?;
            Ok(text)
        })();
        let text = match loaded {
            Ok(text) => text,
            Err(error) => {
                report
                    .warnings
                    .push(format!("{relative}: unreadable: {error}"));
                continue;
            }
        };
        if text.len() > 1024 * 1024 || total_bytes + text.len() > 8 * 1024 * 1024 {
            report.warnings.push(format!(
                "{relative}: composition read budget exceeded (1 MiB/file, 8 MiB total)"
            ));
            continue;
        }
        total_bytes += text.len();
        let is_markup = kind != "codebehind";
        report.files.push(Component {
            path: relative.clone(),
            kind,
            declared_by: parent,
            source_hash: engram_core::ContentHash::compute(text.as_bytes()).0,
        });
        if !is_markup {
            continue;
        }
        let code = COMMENTS.replace_all(&text, " ");
        let code = SERVER_CODE.replace_all(&code, " ");
        let mut declared = HashSet::new();
        let mut fills = Vec::new();
        for tag in CONTENT_TAGS.captures_iter(&code) {
            let attrs = attributes(&tag[2]);
            if tag[1].eq_ignore_ascii_case("ContentPlaceHolder") {
                if let Some(id) = attrs.get("id") {
                    declared.insert(id.clone());
                }
            } else if let Some(id) = attrs.get("contentplaceholderid") {
                fills.push(id.clone());
            }
        }
        contents.insert(relative.clone(), (declared, fills));
        for directive in DIRECTIVES.captures_iter(&code) {
            if directives_seen >= 128 {
                report
                    .warnings
                    .push("composition directive discovery truncated at 128".into());
                break;
            }
            directives_seen += 1;
            let attrs = attributes(&directive[2]);
            let name = directive[1].to_ascii_lowercase();
            if include_codebehind && matches!(name.as_str(), "page" | "master" | "control") {
                if let Some(value) = attrs.get("codefile").or_else(|| attrs.get("codebehind")) {
                    match resolve(&root, &app, path.parent().unwrap_or(&root), value) {
                        Ok(target) => queue.push_back((
                            target,
                            "codebehind".into(),
                            relative.clone(),
                            depth + 1,
                        )),
                        Err(error) => report.warnings.push(format!(
                            "{relative}: codebehind '{value}' unresolved: {error}"
                        )),
                    }
                }
            }
            let linked = if include_master && matches!(name.as_str(), "page" | "master") {
                attrs.get("masterpagefile").map(|value| (value, "master"))
            } else if name == "register" {
                if attrs.contains_key("assembly") {
                    report.warnings.push(format!(
                        "{relative}: assembly control registration requires runtime/type evidence"
                    ));
                }
                attrs.get("src").map(|value| (value, "user_control"))
            } else {
                None
            };
            if let Some((value, kind)) = linked {
                match resolve(&root, &app, path.parent().unwrap_or(&root), value) {
                    Ok(target) => {
                        let target_relative = target
                            .strip_prefix(&root)
                            .unwrap()
                            .to_string_lossy()
                            .replace('\\', "/");
                        if kind == "master" {
                            masters.push((relative.clone(), target_relative));
                        }
                        if seen.contains(&target) && kind == "master" {
                            report.warnings.push(format!("{relative}: repeated master target; inspect for a composition cycle"));
                        }
                        queue.push_back((target, kind.into(), relative.clone(), depth + 1));
                    }
                    Err(error) => report
                        .warnings
                        .push(format!("{relative}: {kind} '{value}' unresolved: {error}")),
                }
            }
        }
    }
    for (child, master) in masters {
        if let Some((_, fills)) = contents.get(&child) {
            for placeholder in fills {
                if report.bindings.len() >= 128 {
                    report
                        .warnings
                        .push("placeholder bindings truncated at 128".into());
                    break;
                }
                let status = match contents.get(&master) {
                    Some((declared, _)) if declared.contains(placeholder) => {
                        "exact_declaration_match"
                    }
                    Some(_) => "no_exact_declaration",
                    None => "master_unavailable",
                };
                report.bindings.push(Binding {
                    child: child.clone(),
                    master: master.clone(),
                    placeholder: placeholder.clone(),
                    status,
                });
            }
        }
    }
    report.warnings.sort();
    report.warnings.dedup();
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nested_master_and_registered_controls_have_source_backed_bindings() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("page.aspx"), "<%@ Page MasterPageFile='~/nested.master' %><%@ Register TagPrefix='u' TagName='Panel' Src='panel.ascx' %><asp:Content ContentPlaceHolderID='Body'>text</asp:Content>").unwrap();
        std::fs::write(root.join("nested.master"), "<%@ Master MasterPageFile='root.master' %><asp:Content ContentPlaceHolderID='Root'><asp:ContentPlaceHolder ID='Body' /></asp:Content>").unwrap();
        std::fs::write(
            root.join("root.master"),
            "<asp:ContentPlaceHolder ID='Root' />",
        )
        .unwrap();
        std::fs::write(
            root.join("panel.ascx"),
            "<%@ Control %><asp:Label ID='Message' />",
        )
        .unwrap();
        let report = collect(root, root, &root.join("page.aspx"), true, true);
        assert_eq!(report.files.len(), 4);
        assert_eq!(report.bindings.len(), 2);
        assert!(
            report
                .bindings
                .iter()
                .all(|binding| binding.status == "exact_declaration_match")
        );
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    }
    #[test]
    fn missing_components_and_master_cycles_remain_visible() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("page.aspx"), "<%@ Page MasterPageFile='a.master' %><%@ Register Src='missing.ascx' %><asp:Content ContentPlaceHolderID='Missing' />").unwrap();
        std::fs::write(
            root.join("a.master"),
            "<%@ Master MasterPageFile='a.master' %>",
        )
        .unwrap();
        let report = collect(root, root, &root.join("page.aspx"), true, true);
        assert_eq!(report.files.len(), 2);
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("cycle"))
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("missing.ascx"))
        );
        assert_eq!(report.bindings[0].status, "no_exact_declaration");
    }

    #[test]
    fn missing_reference_discovery_and_binding_output_are_bounded() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let source = (0..200)
            .map(|index| format!("<%@ Register Src='missing{index}.ascx' %>"))
            .collect::<String>();
        std::fs::write(root.join("page.aspx"), source).unwrap();
        let report = collect(root, root, &root.join("page.aspx"), true, true);
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("truncated at 128"))
        );
        assert!(report.warnings.len() <= 129);
    }
}
