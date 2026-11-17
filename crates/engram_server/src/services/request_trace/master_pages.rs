//! Literal master hosting evidence; source composition is not lifecycle execution.
use super::{
    control_routes, execution,
    routing::{self, Source},
};
use engram_graph::GraphStore;
use regex::Regex;
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::LazyLock,
};

#[derive(Serialize)]
pub struct Binding {
    pub parent_file: String,
    pub parent_source_blake3: String,
    pub directive_line: u32,
    pub directive_excerpt: String,
    pub master_page_file: String,
    pub file: String,
    pub source_blake3: String,
}
#[derive(Serialize)]
pub struct Resolution {
    pub bindings: Vec<Binding>,
    pub gaps: Vec<String>,
    pub truncated: bool,
    pub interpretation: &'static str,
    #[serde(skip)]
    pub(super) sources: Vec<String>,
}

pub(super) fn resolve<'a>(
    graph: &GraphStore,
    project: &str,
    page: Source<'a>,
    sources: &[Source<'a>],
    selected: &BTreeMap<String, u64>,
) -> anyhow::Result<Option<Resolution>> {
    static DIRECTIVE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?is)<%@\s*(?:Page|Master)\b.*?%>").expect("page/master directive")
    });
    let mut result = Resolution {
        bindings: vec![],
        gaps: vec![],
        truncated: false,
        sources: vec![],
        interpretation: "Literal MasterPageFile source links and master Init/Load context only, at most4links with complete directives up to8192bytes. Parent/master file hashes and selected physical revisions are checked. This is not compiler hosting proof, an atomic disk/graph snapshot, effective visibility or execution/lifecycle order. Dynamic master selection, master-control event wiring, PreRender and other lifecycle phases remain unexamined.",
    };
    let mut parent = page;
    let app = routing::app_root(page.path, sources);
    let mut seen = BTreeSet::from([page.path.to_owned()]);
    for depth in 0..=4 {
        let markup = execution::without_markup_comments(parent.content);
        let Some(directive) = DIRECTIVE.find(&markup) else {
            break;
        };
        if directive.as_str().len() > 8192 {
            result.truncated = true;
            result.gaps.push(format!(
                "{}: oversized hosting directive withheld",
                parent.path
            ));
            break;
        }
        let Some(raw) = execution::attribute(directive.as_str(), "MasterPageFile") else {
            break;
        };
        if depth == 4 {
            result.truncated = true;
            result
                .gaps
                .push("Literal master chain capped at4links; remaining hosting unexamined".into());
            break;
        }
        if !control_routes::admitted(graph, project, &parent, selected)? {
            result.gaps.push(format!(
                "{}: parent markup revision/hash not admitted; master context withheld",
                parent.path
            ));
            break;
        }
        let Some(path) = execution::relative_target(parent.path, &raw, app) else {
            result.gaps.push(format!(
                "{}: MasterPageFile lacks literal bounded hosting evidence",
                parent.path
            ));
            break;
        };
        if !path.to_ascii_lowercase().ends_with(".master") {
            result.gaps.push(format!(
                "{}: MasterPageFile is not a literal .master target",
                parent.path
            ));
            break;
        }
        let candidates: Vec<_> = sources
            .iter()
            .filter(|source| source.path.eq_ignore_ascii_case(&path))
            .collect();
        if candidates.len() != 1 {
            result.gaps.push(format!(
                "{}: master {path} has {}source candidates; hosting withheld",
                parent.path,
                candidates.len()
            ));
            break;
        }
        let target = *candidates[0];
        if routing::app_root(target.path, sources) != app
            || !control_routes::admitted(graph, project, &target, selected)?
        {
            result.gaps.push(format!("{}: master source revision/hash or application boundary not admitted; Load context withheld",target.path));
            break;
        }
        if !seen.insert(target.path.to_owned()) {
            result.truncated = true;
            result.gaps.push(format!(
                "{}: repeated literal master path; further hosting withheld",
                target.path
            ));
            break;
        }
        result.bindings.push(Binding {
            parent_file: parent.path.into(),
            parent_source_blake3: blake3::hash(parent.content.as_bytes()).to_hex().to_string(),
            directive_line: markup.as_bytes()[..directive.start()]
                .iter()
                .filter(|byte| **byte == b'\n')
                .count() as u32
                + 1,
            directive_excerpt: directive.as_str().into(),
            master_page_file: raw,
            file: target.path.into(),
            source_blake3: blake3::hash(target.content.as_bytes()).to_hex().to_string(),
        });
        result.sources.push(target.path.into());
        parent = target;
    }
    Ok((!result.bindings.is_empty() || !result.gaps.is_empty()).then_some(result))
}
