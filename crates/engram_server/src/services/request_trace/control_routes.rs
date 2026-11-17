//! Literal user-control instance paths. This is source navigation, not hosting.
use super::{
    execution,
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
    pub instance_path: String,
    pub instance_id: String,
    pub parent_file: String,
    pub parent_source_blake3: String,
    pub instance_line: u32,
    pub registration_line: u32,
    pub instance_excerpt: String,
    pub registration_excerpt: String,
    pub declared_visible: Option<String>,
    pub declared_enabled: Option<String>,
    pub file: String,
    pub source_blake3: String,
}

#[derive(Serialize)]
pub struct Resolution {
    pub requested_control_path: String,
    pub bindings: Vec<Binding>,
    pub terminal_events: Vec<super::control_events::Evidence>,
    pub gaps: Vec<String>,
    pub truncated: bool,
    pub interpretation: &'static str,
    #[serde(skip)]
    pub(super) contexts: Vec<String>,
    #[serde(skip)]
    pub(super) terminals: Vec<String>,
    #[serde(skip)]
    retained_bytes: usize,
}
impl Resolution {
    fn note(&mut self, message: String) {
        if self.gaps.len() < 32 {
            self.gaps.push(message);
        } else {
            self.truncated = true;
        }
    }
}

// Quoted binding expressions can contain '>'; stop only at an unquoted close.
// A complete tag is returned or withheld, never clipped into a declaration.
fn tag_end(markup: &str, start: usize) -> Option<usize> {
    let mut quote = None;
    for (offset, byte) in markup.as_bytes()[start..].iter().take(8192).enumerate() {
        match (quote, *byte) {
            (None, b'\'' | b'"') => quote = Some(*byte),
            (Some(delimiter), value) if delimiter == value => quote = None,
            (None, b'>') => return Some(start + offset + 1),
            _ => {}
        }
    }
    None
}
fn line(markup: &str, start: usize) -> u32 {
    markup.as_bytes()[..start]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count() as u32
        + 1
}

pub(super) fn admitted(
    graph: &GraphStore,
    project: &str,
    source: &Source<'_>,
    selected: &BTreeMap<String, u64>,
) -> anyhow::Result<bool> {
    let Some(revision) = selected.get(source.path) else {
        return Ok(false);
    };
    let Some(file) = graph.get_node(project, &format!("file:{}", source.path))? else {
        return Ok(false);
    };
    Ok(file.generation == *revision
        && file
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("file_hash"))
            .and_then(|hash| hash.as_str())
            .is_some_and(|hash| hash == blake3::hash(source.content.as_bytes()).to_hex().as_str()))
}

struct Registration {
    prefix: String,
    name: String,
    source: String,
    line: u32,
    excerpt: String,
}

pub(super) fn resolve(
    graph: &GraphStore,
    project: &str,
    page: &Source<'_>,
    control_path: &str,
    sources: &[Source<'_>],
    selected: &BTreeMap<String, u64>,
) -> anyhow::Result<Resolution> {
    let mut result = Resolution {
        requested_control_path: control_path.into(),
        bindings: vec![],
        terminal_events: vec![],
        gaps: vec![],
        truncated: false,
        contexts: vec![],
        terminals: vec![],
        retained_bytes: 0,
        interpretation: "Literal same-application Register/Src and server-instance source candidates. Source hashes and selected physical revisions are checked; no atomic graph/disk snapshot, markup compilation, dynamic control creation, master-page composition, effective visibility, lifecycle order, browser postback or runtime execution is proven. Visibility/enabled values are source declarations, not proof an event is reachable. At most8path components,8candidates per level,128register directives,1024instance tags per source and64KiB retained route evidence.",
    };
    let parts: Vec<_> = control_path.split('/').collect();
    anyhow::ensure!(
        (2..=8).contains(&parts.len()),
        "User-control selection requires an instance/child path of2..8components"
    );
    static REGISTER: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?is)<%@\s*Register\b").expect("Register directive"));
    static INSTANCE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"<([A-Za-z_]\w*):([A-Za-z_]\w*)\b").expect("server control tag")
    });
    let mut current = vec![(page.path.to_owned(), String::new())];
    let mut contexts = BTreeSet::new();
    for wanted in &parts[..parts.len() - 1] {
        let mut next = Vec::new();
        let mut next_seen = BTreeSet::new();
        'parents: for (parent_path, parent_instance_path) in current {
            let Some(parent) = sources.iter().find(|source| source.path == parent_path) else {
                continue;
            };
            if !admitted(graph, project, parent, selected)? {
                result.note(format!("{parent_path}: user-control parent markup source/revision not admitted; instance path withheld"));
                continue;
            }
            let markup = execution::without_markup_comments(parent.content);
            let mut registrations = Vec::new();
            for (index, matched) in REGISTER.find_iter(&markup).enumerate() {
                if index == 128 {
                    result.truncated = true;
                    result.note(format!("{parent_path}: Register discovery capped at128; remaining declarations unexamined"));
                    break;
                }
                let Some(end) = tag_end(&markup, matched.start()) else {
                    result.note(format!(
                        "{parent_path}: incomplete/oversized Register declaration withheld"
                    ));
                    continue;
                };
                let text = &markup[matched.start()..end];
                let (Some(prefix), Some(name), Some(source)) = (
                    execution::attribute(text, "TagPrefix"),
                    execution::attribute(text, "TagName"),
                    execution::attribute(text, "Src"),
                ) else {
                    continue;
                };
                registrations.push(Registration {
                    prefix,
                    name,
                    source,
                    line: line(&markup, matched.start()),
                    excerpt: text.into(),
                });
            }
            let parent_hash = blake3::hash(parent.content.as_bytes()).to_hex().to_string();
            for (index, instance) in INSTANCE.captures_iter(&markup).enumerate() {
                if index == 1024 {
                    result.truncated = true;
                    result.note(format!("{parent_path}: instance discovery capped at1024tags; remaining declarations unexamined"));
                    break;
                }
                let matched = instance.get(0).expect("full instance tag");
                let Some(end) = tag_end(&markup, matched.start()) else {
                    result.note(format!(
                        "{parent_path}: incomplete/oversized instance declaration withheld"
                    ));
                    continue;
                };
                let text = &markup[matched.start()..end];
                if !execution::attribute(text, "ID")
                    .is_some_and(|id| id.eq_ignore_ascii_case(wanted))
                    || !execution::attribute(text, "runat")
                        .is_some_and(|value| value.eq_ignore_ascii_case("server"))
                {
                    continue;
                }
                for registration in registrations.iter().filter(|registration| {
                    registration.prefix.eq_ignore_ascii_case(&instance[1])
                        && registration.name.eq_ignore_ascii_case(&instance[2])
                }) {
                    let Some(path) = execution::relative_target(
                        parent.path,
                        &registration.source,
                        routing::app_root(parent.path, sources),
                    ) else {
                        result.note(format!("{parent_path}: {wanted} Src cannot be resolved from literal hosting evidence"));
                        continue;
                    };
                    if !path.to_ascii_lowercase().ends_with(".ascx") {
                        result.note(format!(
                            "{parent_path}: {wanted} Src is not a literal ASCX target"
                        ));
                        continue;
                    }
                    let targets: Vec<_> = sources
                        .iter()
                        .filter(|source| source.path.eq_ignore_ascii_case(&path))
                        .collect();
                    if targets.is_empty() {
                        result.note(format!(
                            "{parent_path}: {wanted} target {path} is absent from selected source"
                        ));
                    }
                    for target in targets {
                        if routing::app_root(target.path, sources)
                            != routing::app_root(parent.path, sources)
                            || !admitted(graph, project, target, selected)?
                        {
                            result.note(format!("{}: control markup source/revision or application boundary not admitted; instance withheld",target.path));
                            continue;
                        }
                        let instance_path = if parent_instance_path.is_empty() {
                            wanted.to_string()
                        } else {
                            format!("{parent_instance_path}/{wanted}")
                        };
                        let key = (target.path.to_owned(), instance_path.clone());
                        if !next_seen.contains(&key) && next.len() == 8 {
                            result.truncated = true;
                            result.note("User-control routing capped at8source candidates per level; remaining branches unexamined".into());
                            break 'parents;
                        }
                        let bytes = text.len()
                            + registration.excerpt.len()
                            + parent.path.len()
                            + target.path.len()
                            + 512;
                        if result.retained_bytes + bytes > 64 * 1024 {
                            result.truncated = true;
                            result.note("User-control route evidence reached64KiB; remaining bindings withheld".into());
                            break 'parents;
                        }
                        result.retained_bytes += bytes;
                        result.bindings.push(Binding {
                            instance_path: instance_path.clone(),
                            instance_id: wanted.to_string(),
                            parent_file: parent.path.into(),
                            parent_source_blake3: parent_hash.clone(),
                            instance_line: line(&markup, matched.start()),
                            registration_line: registration.line,
                            instance_excerpt: text.into(),
                            registration_excerpt: registration.excerpt.clone(),
                            declared_visible: execution::attribute(text, "Visible"),
                            declared_enabled: execution::attribute(text, "Enabled"),
                            file: target.path.into(),
                            source_blake3: blake3::hash(target.content.as_bytes())
                                .to_hex()
                                .to_string(),
                        });
                        contexts.insert(target.path.to_owned());
                        if next_seen.insert(key.clone()) {
                            next.push(key);
                        }
                    }
                }
            }
        }
        if next.is_empty() {
            result.note(format!("No admitted literal user-control instance for path component {wanted}; later components unexamined"));
            current = next;
            break;
        }
        if next.len() > 1 {
            result.note(format!("Path component {wanted} has {}source candidates; no unique runtime instance is inferred",next.len()));
        }
        current = next;
    }
    result.contexts = contexts.into_iter().collect();
    result.terminals = current
        .into_iter()
        .map(|(file, _)| file)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    Ok(result)
}
