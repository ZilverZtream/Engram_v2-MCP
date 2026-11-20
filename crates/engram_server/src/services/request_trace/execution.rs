//! Assemble source-bound static request paths using the existing graph and flow reader.
use super::routing::{RouteCandidate, Source};
use engram_graph::{EdgeKind, GraphStore, Node};
use regex::Regex;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

const MAX_METHODS: usize = 256;
const MAX_NODES: usize = 64;
const MAX_DEPTH: usize = 6;
const MAX_EDGES: usize = 128;
const MAX_REDIRECTS_PER_METHOD: usize = 8;
const MAX_STARTUP_SCRIPTS_PER_METHOD: usize = 8;
const MAX_RESPONSE_METADATA_PER_METHOD: usize = 8;
const MAX_RESPONSE_BODY_WRITES_PER_METHOD: usize = 8;

pub(super) fn response_redirects(
    tokens: &[super::routing::Token],
    start_line: u32,
) -> Option<serde_json::Value> {
    use serde_json::json;

    let mut candidates = Vec::new();
    let mut truncated = false;
    for call in tokens.windows(6) {
        if call[..4].iter().any(|token| token.string)
            || !call[0].text.eq_ignore_ascii_case("Response")
            || call[1].text != "."
            || !matches!(
                call[2].text.to_ascii_lowercase().as_str(),
                "redirect" | "redirectpermanent"
            )
            || call[3].text != "("
        {
            continue;
        }
        if candidates.len() == MAX_REDIRECTS_PER_METHOD {
            truncated = true;
            break;
        }
        let literal = (call[4].string && !call[5].string && matches!(call[5].text.as_str(), ")" | ","))
            .then_some(call[4].text.as_str());
        candidates.push(json!({
            "line": start_line as usize + call[0].line - 1,
            "operation": call[2].text,
            "destination": literal.filter(|value| value.len() <= 512),
            "destination_status": match literal {
                Some(value) if value.len() <= 512 => "literal",
                Some(_) => "literal_omitted",
                None => "unresolved_expression",
            },
        }));
    }
    (!candidates.is_empty()).then(|| json!({
        "candidates": candidates,
        "truncated": truncated,
        "interpretation": "VB source call spellings. Receiver binding, execution and HTTP status are unverified.",
    }))
}

pub(super) fn startup_script_registrations(
    tokens: &[super::routing::Token],
    start_line: u32,
) -> Option<serde_json::Value> {
    use super::routing::{Token, arguments};
    use serde_json::json;

    fn literal(tokens: &[Token]) -> Option<&str> {
        match tokens {
            [token] if token.string => Some(token.text.as_str()),
            _ => None,
        }
    }

    let mut candidates = Vec::new();
    let mut truncated = false;
    for (index, call) in tokens.windows(4).enumerate() {
        if call.iter().any(|token| token.string)
            || !call[0].text.eq_ignore_ascii_case("ScriptManager")
            || call[1].text != "."
            || !call[2].text.eq_ignore_ascii_case("RegisterStartupScript")
            || call[3].text != "("
        {
            continue;
        }
        let Some(args) = arguments(&tokens[index + 4..]) else {
            continue;
        };
        let [_, _, key, script, _] = args.as_slice() else {
            continue;
        };
        if candidates.len() == MAX_STARTUP_SCRIPTS_PER_METHOD {
            truncated = true;
            break;
        }
        let script = literal(script);
        candidates.push(json!({
            "line": start_line as usize + call[0].line - 1,
            "key": literal(key).filter(|value| value.len() <= 256),
            "script_status": if script.is_some() { "literal" } else { "unresolved_expression" },
        }));
    }
    (!candidates.is_empty()).then(|| json!({
        "candidates": candidates,
        "truncated": truncated,
        "interpretation": "Authored VB registrations. The generated script, browser execution and effects are unverified.",
    }))
}

pub(super) fn response_metadata(
    tokens: &[super::routing::Token],
    start_line: u32,
) -> Option<serde_json::Value> {
    use super::routing::arguments;
    use serde_json::json;

    let mut candidates = Vec::new();
    let mut truncated = false;
    for (index, call) in tokens.windows(5).enumerate() {
        if call[..4].iter().any(|token| token.string)
            || !call[0].text.eq_ignore_ascii_case("Response")
            || call[1].text != "."
        {
            continue;
        }
        let line = start_line as usize + call[0].line - 1;
        let operation = &call[2].text;
        let candidate = match operation.to_ascii_lowercase().as_str() {
            member @ ("statuscode" | "contenttype") if call[3].text == "=" => {
                // Require a statement start so comparisons are not assignments.
                if index > 0 && tokens[index - 1].line == call[0].line
                    && tokens[index - 1].text != ":"
                {
                    continue;
                }
                let value = &call[4];
                let single_value = tokens.get(index + 5).is_none_or(|next| {
                    next.line > value.line || (!next.string && next.text == ":")
                });
                let literal = if single_value && member == "statuscode" && !value.string {
                    value.text.parse::<u16>().ok()
                        .filter(|code| (100..=599).contains(code)).map(|code| json!(code))
                } else if single_value && member == "contenttype" && value.string
                    && value.text.len() <= 256
                {
                    Some(json!(value.text))
                } else {
                    None
                };
                json!({"line":line,"member":operation,"operation":"assignment","value":literal,
                       "value_status":if literal.is_some() {"literal"} else {"unresolved_expression"}})
            }
            "appendheader" | "addheader" if call[3].text == "(" => {
                let Some(args) = arguments(&tokens[index + 4..]) else {
                    continue;
                };
                let [name, _] = args.as_slice() else {
                    continue;
                };
                let literal_name = match *name {
                    [name] if name.string && name.text.len() <= 256 => Some(name.text.as_str()),
                    _ => None,
                };
                json!({"line":line,"member":operation,"operation":"header_call","name":literal_name,
                       "name_status":if literal_name.is_some() {"literal"} else {"unresolved_expression"}})
            }
            _ => continue,
        };
        if candidates.len() == MAX_RESPONSE_METADATA_PER_METHOD {
            truncated = true;
            break;
        }
        candidates.push(candidate);
    }
    (!candidates.is_empty()).then(|| json!({
        "candidates": candidates,
        "truncated": truncated,
        "interpretation": "Authored VB response statements. Values are lexical source observations. Final status, headers and execution are unverified.",
    }))
}

fn response_body_writes(
    tokens: &[super::routing::Token],
    start_line: u32,
) -> Option<serde_json::Value> {
    use serde_json::json;

    let mut candidates = Vec::new();
    let mut truncated = false;
    for (index, call) in tokens.windows(4).enumerate() {
        if call.iter().any(|token| token.string)
            || !call[0].text.eq_ignore_ascii_case("Response")
            || call[1].text != "."
        {
            continue;
        }
        let kind = match call[2].text.to_ascii_lowercase().as_str() {
            "transmitfile" | "writefile" if call[3].text == "(" => "file",
            "binarywrite" if call[3].text == "(" => "binary",
            "write" if call[3].text == "(" => "response_write",
            "outputstream" if call[3].text == "." => {
                let Some([write, open]) = tokens.get(index + 4..index + 6) else {
                    continue;
                };
                if write.string || open.string
                    || !write.text.eq_ignore_ascii_case("Write") || open.text != "("
                {
                    continue;
                }
                "stream"
            }
            _ => continue,
        };
        if candidates.len() == MAX_RESPONSE_BODY_WRITES_PER_METHOD {
            truncated = true;
            break;
        }
        let operation = if kind == "stream" {
            format!("{}.{}", call[2].text, tokens[index + 4].text)
        } else {
            call[2].text.clone()
        };
        candidates.push(json!({
            "line": start_line as usize + call[0].line - 1,
            "operation": operation,
            "kind": kind,
        }));
    }
    (!candidates.is_empty()).then(|| json!({
        "candidates": candidates,
        "truncated": truncated,
        "interpretation": "Authored VB response body calls. Receiver binding, argument values and emitted content remain unresolved.",
    }))
}

#[derive(Serialize)]
pub struct Handler {
    pub node_id: String,
    pub name: String,
    pub file: String,
    pub line: u32,
    pub binding: String,
}

#[derive(Serialize)]
pub struct Hop {
    pub from: String,
    pub to: String,
    pub kind: String,
    pub target_name: String,
    pub target_file: Option<String>,
    pub target_line: Option<u32>,
    pub target_source_verified: bool,
    pub evidence: Option<serde_json::Value>,
    /// An authored mapping is distinct from a verified physical table definition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub table_mapping: Option<serde_json::Value>,
}

#[derive(Serialize)]
pub struct PageTrace {
    pub page: String,
    pub codebehind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control_event: Option<super::control_events::Evidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control_routes: Option<super::control_routes::Resolution>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub master_pages: Option<super::master_pages::Resolution>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_context: Option<super::base_pages::Resolution>,
    pub handlers: Vec<Handler>,
    pub paths: Vec<Hop>,
    pub source_members: Vec<serde_json::Value>,
    pub gaps: Vec<String>,
    pub trigger_coverage: &'static str,
    pub interpretation: &'static str,
}

pub(super) fn attribute(source: &str, key: &str) -> Option<String> {
    let re = Regex::new(&format!(
        r#"(?is)\b{}\s*=\s*(?:"([^"]*)"|'([^']*)')"#,
        regex::escape(key)
    ))
    .ok()?;
    let captures = re.captures(source)?;
    Some(
        captures
            .get(1)
            .or_else(|| captures.get(2))?
            .as_str()
            .to_owned(),
    )
}

pub(super) fn without_markup_comments(source: &str) -> String {
    Regex::new(r"(?s)<%--.*?--%>|<!--.*?-->")
        .expect("comment syntax")
        .replace_all(source, |c: &regex::Captures<'_>| {
            c[0].chars()
                .map(|v| if v == '\n' { '\n' } else { ' ' })
                .collect::<String>()
        })
        .into_owned()
}

pub(super) fn relative_target(page: &str, raw: &str, app_root: Option<&str>) -> Option<String> {
    let raw = raw.replace('\\', "/");
    if raw.contains([':', '\0', '%']) || raw.starts_with('/') {
        return None;
    }
    let joined = if let Some(target) = raw.strip_prefix("~/") {
        format!("{}/{target}", app_root?)
    } else {
        format!(
            "{}/{}",
            page.rsplit_once('/').map_or("", |(parent, _)| parent),
            raw
        )
    };
    let mut parts = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => (),
            ".." => {
                parts.pop()?;
            }
            value => parts.push(value),
        }
    }
    Some(parts.join("/"))
}

pub(super) fn member_name(node: &Node) -> &str {
    node.name
        .rsplit(['.', ':'])
        .next()
        .unwrap_or(&node.name)
        .split('(')
        .next()
        .unwrap_or(&node.name)
}

/// Every expansion requires a graph file fingerprint and the selected physical
/// revision. Matching current disk text alone cannot authorize a newer graph view.
pub(crate) fn verified_node(
    graph: &GraphStore,
    project: &str,
    node: &Node,
    sources: &[Source<'_>],
    selected: &BTreeMap<String, u64>,
) -> anyhow::Result<bool> {
    let path = node.file_path.as_str();
    let Some(revision) = selected.get(path) else {
        return Ok(false);
    };
    if node.generation != *revision {
        return Ok(false);
    }
    let Some(source) = sources.iter().find(|source| source.path == path) else {
        return Ok(false);
    };
    if node.start_line == 0
        || node.end_line < node.start_line
        || node.end_line as usize > source.content.lines().count()
    {
        return Ok(false);
    }
    let Some(file) = graph.get_node(project, &format!("file:{path}"))? else {
        return Ok(false);
    };
    Ok(file.generation == *revision
        && file
            .metadata
            .as_ref()
            .and_then(|v| v.get("file_hash"))
            .and_then(|v| v.as_str())
            .is_some_and(|hash| hash == blake3::hash(source.content.as_bytes()).to_hex().as_str()))
}

/// A source-authored binding, not a suffix-based guess that every *_Click runs.
pub(super) fn handler_bindings(
    markup: &str,
    source: &str,
    methods: &[Node],
    action: Option<&str>,
    control_event: Option<&super::control_events::Evidence>,
) -> Vec<(Node, String)> {
    let requested = action.map(str::trim).filter(|a| !a.is_empty());
    let click = requested.and_then(|a| a.strip_prefix("click "));
    let form = requested.is_some_and(|a| a.eq_ignore_ascii_case("submit form"));
    let lifecycle_context =
        requested.is_none_or(|a| a.eq_ignore_ascii_case("load page")) || click.is_some() || form || control_event.is_some();
    // Markup observations belong to the page, not each candidate method. In
    // particular, avoid compiling attribute readers for every tag/method pair.
    let click_bindings: Vec<_> = if click.is_some() || form {
        super::control_events::start_tags(markup).filter_map(|tag| {
            let mut id = None;
            let mut handler = None;
            for (name, value) in super::control_events::attributes(tag.as_str()) {
                if name.eq_ignore_ascii_case("ID") {
                    id = Some(value);
                } else if name.eq_ignore_ascii_case("OnClick") {
                    handler = Some(value);
                }
            }
            if !form && !click.is_some_and(|target| id.is_some_and(|id| id.eq_ignore_ascii_case(target))) {
                return None;
            }
            Some((id.unwrap_or("unnamed control"), handler?))
        }).collect()
    } else { Vec::new() };
    let auto_event_wireup = lifecycle_context && attribute(markup, "AutoEventWireup")
        .is_some_and(|value| value.eq_ignore_ascii_case("true"));
    let mut selected = Vec::new();
    for node in methods {
        let name = member_name(node);
        let mut reasons = Vec::new();
        // Header and Handles are confined to the indexed member's first line.
        // Multiline/dynamic bindings remain explicit gaps at the page level.
        let header = source
            .lines()
            .nth(node.start_line.saturating_sub(1) as usize)
            .unwrap_or("");
        let tokens =
            super::routing::tokens(header, node.language == "vb" || node.language == "vbnet");
        if let Some(position) = tokens
            .iter()
            .position(|token| !token.string && token.text.eq_ignore_ascii_case("Handles"))
        {
            for binding in tokens[position + 1..].split(|token| !token.string && token.text == ",")
            {
                if binding.len() != 3
                    || binding.iter().any(|token| token.string)
                    || binding[1].text != "."
                {
                    continue;
                }
                let control = binding[0].text.as_str();
                let event = binding[2].text.as_str();
                if (lifecycle_context
                    && matches!(control.to_ascii_lowercase().as_str(), "me" | "mybase")
                    && ["PreInit", "Init", "Load", "LoadComplete", "PreRender"]
                        .iter()
                        .any(|phase| event.eq_ignore_ascii_case(phase)))
                    || click.is_some_and(|target| {
                        control.eq_ignore_ascii_case(target) && event.eq_ignore_ascii_case("click")
                    })
                    || control_event.is_some_and(|request| control.eq_ignore_ascii_case(&request.control)
                        && event.eq_ignore_ascii_case(&request.event))
                {
                    reasons.push(format!("Handles {control}.{event}"));
                }
            }
        }
        if let Some(request) = control_event {
            for binding in &request.markup_bindings {
                if binding.handler.eq_ignore_ascii_case(name) {
                    reasons.push(format!("{} {}={name} (authored server-control event)", request.control, binding.attribute));
                }
            }
            for subscription in &request.vb_subscriptions {
                if subscription.handler.eq_ignore_ascii_case(name) {
                    reasons.push(format!(
                        "AddHandler {}.{}, AddressOf {name} ({}:{})",
                        request.control, request.event, subscription.file, subscription.line
                    ));
                }
            }
        }
        for (id, handler) in &click_bindings {
            if handler.eq_ignore_ascii_case(name) {
                reasons.push(format!("{id} OnClick={name}"));
            }
        }
        if auto_event_wireup
            && (name.eq_ignore_ascii_case("Page_PreInit") || name.eq_ignore_ascii_case("Page_Init") || name.eq_ignore_ascii_case("Page_Load") || name.eq_ignore_ascii_case("Page_PreRender"))
        {
            reasons.push(format!("{name} with authored AutoEventWireup=true"));
        }
        // An explicit method asks for that source entry, but does not prove HTTP dispatch.
        if requested.is_some_and(|value| value.eq_ignore_ascii_case(name)) {
            reasons.push("explicit method selection; HTTP event binding not established".into());
        }
        if !reasons.is_empty() {
            selected.push((node.clone(), reasons.join("; ")));
        }
    }
    selected
}

pub fn trace_page(
    graph: &Arc<GraphStore>,
    project: &str,
    route: &RouteCandidate,
    action: Option<&str>,
    include_db: bool,
    include_triggers: bool,
    sources: &[Source<'_>],
    selected: &BTreeMap<String, u64>,
) -> anyhow::Result<PageTrace> {
    if route.page.to_ascii_lowercase().ends_with(".ashx") {
        return super::http_handlers::trace(
            graph, project, route, action, include_db, include_triggers, sources, selected,
        );
    }
    let requested_event = super::control_events::requested(action)?;
    if route.web_method.is_some() {
        return super::web_methods::trace(graph, project, route, action, include_db, include_triggers, sources, selected);
    }
    let mut result = empty_trace(&route.page, include_db, include_triggers);
    let Some(page) = sources.iter().find(|source| source.path == route.page) else {
        result.gaps.push("Page source unavailable".into());
        return Ok(result);
    };
    if let Some((control, event)) = requested_event.filter(|(control, _)| control.contains('/')) {
        return trace_control_instances(graph, project, result, *page, control, event,
            include_db, sources, selected);
    }
    let mut master_entries=Vec::new();
    add_master_context(graph,project,&mut result,*page,&mut master_entries,sources,selected)?;
    let finish=|result,chosen|trace_selected(graph,project,result,chosen,include_db,sources,selected);
    let markup = without_markup_comments(page.content);
    let directive =
        Regex::new(r"(?is)<%@\s*(?:Page|WebHandler|WebService)\b.*?%>").expect("directive syntax");
    let Some(directive) = directive.find(&markup) else {
        result
            .gaps
            .push("Page/handler directive unavailable".into());
        return finish(result,master_entries);
    };
    let Some(raw) = attribute(directive.as_str(), "CodeFile")
        .or_else(|| attribute(directive.as_str(), "CodeBehind"))
    else {
        result.gaps.push("No authored code-behind directive; inline/inherited handler resolution is not established".into());
        return finish(result,master_entries);
    };
    let root = super::routing::app_root(page.path, sources);
    let Some(path) = relative_target(page.path, &raw, root) else {
        result
            .gaps
            .push("Code-behind target escapes the project or lacks hosting evidence".into());
        return finish(result,master_entries);
    };
    let matches: Vec<_> = sources
        .iter()
        .filter(|source| source.path.eq_ignore_ascii_case(&path))
        .collect();
    if matches.len() != 1 {
        result.gaps.push(format!(
            "Code-behind {path} has {} selected source matches",
            matches.len()
        ));
        return finish(result,master_entries);
    }
    let source = matches[0];
    result.codebehind = Some(source.path.into());
    let mut methods =
        graph.query_nodes_in_file(project, Some("function"), source.path, MAX_METHODS + 1)?;
    if methods.len() > MAX_METHODS {
        methods.truncate(MAX_METHODS);
        result
            .gaps
            .push(format!("Method inventory truncated at {MAX_METHODS}"));
    }
    result.control_event = requested_event
        .map(|(control, event)| super::control_events::collect(*page, *source, control, event));
    let mut chosen = handler_bindings(&markup, source.content, &methods, action, result.control_event.as_ref());
    add_base_context(graph,project,&mut result,*page,*source,&mut chosen,sources,selected)?;
    if chosen.is_empty() {
        result.gaps.push("No requested authored handler binding found; dynamic, multiline and inherited bindings are not inferred".into());
    }
    if action.is_some_and(|a| a.eq_ignore_ascii_case("submit form")) {
        result.gaps.push("Submit control unspecified; returned click handlers are alternatives, not an execution sequence".into());
    }
    for (node,binding) in master_entries { merge_context_entry(&mut chosen,node,binding,&mut result.gaps); }
    finish(result,chosen)
}

fn merge_context_entry(chosen:&mut Vec<(Node,String)>,node:Node,binding:String,gaps:&mut Vec<String>) {
    if let Some((_,existing))=chosen.iter_mut().find(|(existing,_)|existing.node_id==node.node_id) {
        if !existing.contains(&binding) {existing.push_str("; ");existing.push_str(&binding);}
    } else if chosen.len()<MAX_NODES {chosen.push((node,binding));}
    else if !gaps.iter().any(|gap|gap.starts_with("Master context entry cap")) {
        gaps.push(format!("Master context entry cap{MAX_NODES} reached; remaining entries unexamined"));
    }
}

fn add_base_context<'a>(graph:&Arc<GraphStore>,project:&str,result:&mut PageTrace,page:Source<'a>,code:Source<'a>,chosen:&mut Vec<(Node,String)>,sources:&[Source<'a>],selected:&BTreeMap<String,u64>)->anyhow::Result<()> {
    let (context,entries)=super::base_pages::collect(graph,project,page,code,sources,selected)?;
    for (node,binding) in entries {merge_context_entry(chosen,node,binding,&mut result.gaps);}
    result.base_context=context;Ok(())
}

fn add_master_context<'a>(graph:&Arc<GraphStore>,project:&str,result:&mut PageTrace,page:Source<'a>,chosen:&mut Vec<(Node,String)>,sources:&[Source<'a>],selected:&BTreeMap<String,u64>)->anyhow::Result<()> {
    let Some(mut plan)=super::master_pages::resolve(graph,project,page,sources,selected)? else {return Ok(())};
    for path in &plan.sources {
        let Some(master)=sources.iter().find(|source|source.path==path) else {continue};
        let Some(code)=control_codebehind(*master,sources,&mut plan.gaps) else {continue};
        if !super::control_routes::admitted(graph,project,&code,selected)? {
            plan.gaps.push(format!("{}: master code-behind revision/hash not admitted; lifecycle entries withheld",code.path));continue;
        }
        let mut methods=graph.query_nodes_in_file(project,Some("function"),code.path,MAX_METHODS+1)?;
        if methods.len()>MAX_METHODS {methods.truncate(MAX_METHODS);plan.truncated=true;plan.gaps.push(format!("{}: master method inventory capped at{MAX_METHODS}",code.path));}
        let markup=without_markup_comments(master.content);
        for (node,binding) in handler_bindings(&markup,code.content,&methods,Some("load page"),None) {
            merge_context_entry(chosen,node,format!("master source context {}: {binding}; not lifecycle order",master.path),&mut result.gaps);
        }
    }
    result.master_pages=Some(plan);Ok(())
}

pub(super) fn control_codebehind<'a>(markup_source: Source<'a>, sources: &[Source<'a>], gaps: &mut Vec<String>) -> Option<Source<'a>> {
    let markup=without_markup_comments(markup_source.content);
    let directive=Regex::new(r"(?is)<%@\s*(?:Page|Control|Master)\b.*?%>").expect("page/control/master directive");
    let Some(directive)=directive.find(&markup) else {
        gaps.push(format!("{}: authored Page/Control directive unavailable",markup_source.path));return None;
    };
    let Some(raw)=attribute(directive.as_str(),"CodeFile").or_else(||attribute(directive.as_str(),"CodeBehind")) else {
        gaps.push(format!("{}: no literal code-behind; inline/inherited handlers unexamined",markup_source.path));return None;
    };
    let Some(path)=relative_target(markup_source.path,&raw,super::routing::app_root(markup_source.path,sources)) else {
        gaps.push(format!("{}: code-behind target lacks bounded hosting evidence",markup_source.path));return None;
    };
    let matches:Vec<_>=sources.iter().filter(|source|source.path.eq_ignore_ascii_case(&path)
        && super::routing::app_root(source.path,sources)==super::routing::app_root(markup_source.path,sources)).collect();
    if matches.len()!=1 {
        gaps.push(format!("{}: code-behind {path} has {}same-application source candidates; handlers withheld",markup_source.path,matches.len()));return None;
    }
    Some(*matches[0])
}

fn trace_control_instances<'a>(
    graph: &Arc<GraphStore>, project: &str, mut result: PageTrace, page: Source<'a>,
    control_path: &str, event: &str, include_db: bool, sources: &[Source<'a>],
    selected: &BTreeMap<String,u64>,
) -> anyhow::Result<PageTrace> {
    let mut routes=super::control_routes::resolve(graph,project,&page,control_path,sources,selected)?;
    let local=control_path.rsplit('/').next().expect("validated child selector");
    let terminal_action=format!("event {local}.{event}");
    let mut markup_files=vec![page];
    markup_files.extend(routes.contexts.iter().filter_map(|path|sources.iter().find(|source|source.path==path).copied()));
    let mut chosen:Vec<(Node,String)>=Vec::new();
    let mut positions:BTreeMap<String,usize>=BTreeMap::new();
    for markup_source in markup_files {
        let Some(code)=control_codebehind(markup_source,sources,&mut result.gaps) else {continue};
        if markup_source.path==page.path {
            result.codebehind=Some(code.path.into());
            add_base_context(graph,project,&mut result,page,code,&mut chosen,sources,selected)?;
            for (position,(node,_)) in chosen.iter().enumerate() {positions.insert(node.node_id.clone(),position);}
        }
        let mut methods=graph.query_nodes_in_file(project,Some("function"),code.path,MAX_METHODS+1)?;
        if methods.len()>MAX_METHODS {methods.truncate(MAX_METHODS);result.gaps.push(format!("{}: method inventory truncated at{MAX_METHODS}",code.path));}
        let is_terminal=routes.terminals.iter().any(|path|path==markup_source.path);
        let evidence=is_terminal.then(||super::control_events::collect(markup_source,code,local,event));
        let markup=without_markup_comments(markup_source.content);
        let entries=handler_bindings(&markup,code.content,&methods,
            Some(if is_terminal {&terminal_action} else {"load page"}),evidence.as_ref());
        if let Some(evidence)=evidence {routes.terminal_events.push(evidence);}
        for (node,binding) in entries {
            let binding=format!("control path {control_path}: {binding}; source-level context, not lifecycle order");
            if let Some(&position)=positions.get(&node.node_id) {
                let existing:&mut (Node,String)=&mut chosen[position];
                if !existing.1.contains(&binding) {existing.1.push_str("; ");existing.1.push_str(&binding);}
            } else if chosen.len()<MAX_NODES {
                positions.insert(node.node_id.clone(),chosen.len());chosen.push((node,binding));
            } else if !result.gaps.iter().any(|gap|gap.starts_with("Control entry inventory")) {
                result.gaps.push(format!("Control entry inventory capped at{MAX_NODES}; further entries unexamined"));
            }
        }
    }
    if routes.terminals.is_empty() {result.gaps.push("No complete admitted control-instance route; inspect control_routes gaps".into());}
    if chosen.is_empty() {result.gaps.push("No admitted page/control lifecycle or terminal event entry found".into());}
    add_master_context(graph,project,&mut result,page,&mut chosen,sources,selected)?;
    result.control_routes=Some(routes);
    trace_selected(graph,project,result,chosen,include_db,sources,selected)
}

pub(super) fn empty_trace(path: &str, include_db: bool, include_triggers: bool) -> PageTrace {
    PageTrace {
        page: path.into(),
        codebehind: None,
        control_event: None,
        control_routes: None,
        master_pages: None,
        base_context: None,
        handlers: Vec::new(),
        paths: Vec::new(),
        source_members: Vec::new(),
        gaps: Vec::new(),
        trigger_coverage: if !include_db {
            "not_requested_include_db_false"
        } else if !include_triggers {
            "not_requested"
        } else {
            "unknown_no_trigger_binding_in_existing_graph"
        },
        interpretation: "Static possible paths. Source order is not execution, branch reachability, successful completion, HTTP status or proof that a trigger fires. Calls and database targets retain their indexed evidence.",
    }
}

pub(super) fn trace_selected(
    graph: &Arc<GraphStore>,
    project: &str,
    mut result: PageTrace,
    chosen: Vec<(Node, String)>,
    include_db: bool,
    sources: &[Source<'_>],
    selected: &BTreeMap<String, u64>,
) -> anyhow::Result<PageTrace> {
    let mut queue = VecDeque::new();
    for (node, binding) in chosen {
        if !verified_node(graph, project, &node, sources, selected)? {
            result.gaps.push(format!(
                "{}: graph/source revision or fingerprint unavailable or changed; handler withheld",
                node.node_id
            ));
            continue;
        }
        result.handlers.push(Handler {
            node_id: node.node_id.clone(),
            name: member_name(&node).into(),
            file: node.file_path.to_string(),
            line: node.start_line,
            binding,
        });
        queue.push_back((node, 0));
    }
    let database = include_db.then(|| super::database::Catalog::new(sources));
    let mappings = if include_db {
        Some(super::dbml::Catalog::new(
            graph, project, sources, selected,
        )?)
    } else {
        None
    };
    if let Some(mappings) = &mappings {
        result.gaps.extend(mappings.gaps.iter().cloned());
    }
    if let Some(database) = &database {
        result.gaps.extend(database.gaps.iter().cloned());
        result.gaps.push("Database coverage is partial: source fallback covers local literal SqlCommand stored-procedure candidates, bounded literal EXEC/EXECUTE procedure references, literal VB ExecuteCommand/ExecuteQuery SQL, source SqlCommand text candidates with stable Const/Static initializers, and explicit local VB context/member DBML mappings. Dynamic commands, generated SQL/ORM translation, aliases, deployed database identity and runtime or recursive procedure execution are not resolved.".into());
    }
    let mut visited = BTreeSet::new();
    let mut control_calls = super::control_calls::Resolver::new(
        graph, project, &result.page, result.codebehind.as_deref(), sources, selected,
    );
    while let Some((node, depth)) = queue.pop_front() {
        if !visited.insert(node.node_id.clone()) {
            continue;
        }
        if visited.len() > MAX_NODES {
            result.gaps.push(format!(
                "Call/data expansion truncated at {MAX_NODES} nodes"
            ));
            break;
        }
        if depth >= MAX_DEPTH {
            result.gaps.push(format!(
                "{}: expansion stopped at depth {MAX_DEPTH}",
                node.node_id
            ));
            continue;
        }
        if !verified_node(graph, project, &node, sources, selected)? {
            result.gaps.push(format!("{}: target source unavailable or differs from selected graph revision; expansion stopped", node.node_id));
            continue;
        }
        if let Some(source) = sources
            .iter()
            .find(|source| source.path == node.file_path.as_str())
        {
            let mut excerpt = String::new();
            let mut shown = 0;
            let lines = node
                .end_line
                .saturating_sub(node.start_line)
                .saturating_add(1) as usize;
            for line in source
                .content
                .lines()
                .skip(node.start_line.saturating_sub(1) as usize)
                .take(lines)
            {
                if excerpt.len() + line.len() + 1 > 2048 {
                    break;
                }
                excerpt.push_str(line);
                excerpt.push('\n');
                shown += 1;
            }
            let source_blake3 = blake3::hash(source.content.as_bytes()).to_hex().to_string();
            let mut member = serde_json::json!({"node_id":node.node_id,"file":source.path,"line_start":node.start_line,
                "line_end":node.end_line,"source_blake3":source_blake3,
                "excerpt":excerpt,"shown_lines":shown,"excerpt_truncated":shown<lines,
                "interpretation":"Authored code, including guards and response expressions. Source order does not prove an operation executes or completes.",
                "recovery":{"tool":"get_full_method_body","arguments":{"project_id":project,"file_path":source.path,
                    "line_start":node.start_line,"context_lines":0,"expected_file_blake3":source_blake3}}});
            let vb = source.path.to_ascii_lowercase().ends_with(".vb");
            let body = (node.node_type == "function" && (vb || database.is_some())).then(|| {
                source
                    .content
                    .lines()
                    .skip(node.start_line.saturating_sub(1) as usize)
                    .take(lines)
                    .collect::<Vec<_>>()
                    .join("\n")
            });
            if vb && let Some(body) = body.as_deref() {
                let tokens = super::routing::tokens(body, true);
                if let Some(redirects) = response_redirects(&tokens, node.start_line) {
                    member["response_redirects"] = redirects;
                }
                if let Some(scripts) = startup_script_registrations(&tokens, node.start_line) {
                    member["startup_script_registrations"] = scripts;
                }
                if let Some(metadata) = response_metadata(&tokens, node.start_line) {
                    member["response_metadata"] = metadata;
                }
                if let Some(writes) = response_body_writes(&tokens, node.start_line) {
                    member["response_body_writes"] = writes;
                }
            }
            result.source_members.push(member);
            if let (Some(database), Some(body)) = (&database, body.as_deref()) {
                super::inline_sql::append(
                    &node, source, body, &mut result.paths, &mut result.gaps, MAX_EDGES,
                );
                super::sql_commands::append(
                    &node, source, body, &mut result.paths, &mut result.gaps, MAX_EDGES,
                );
                database.append(
                    project,
                    &node,
                    body,
                    &mut result.paths,
                    &mut result.source_members,
                    &mut result.gaps,
                    MAX_EDGES,
                );
            }
        }
        let mut kinds = vec![
            EdgeKind::Calls,
            EdgeKind::ReadsState,
            EdgeKind::WritesState,
            EdgeKind::DataBinding,
            EdgeKind::ApiCall,
        ];
        if include_db {
            kinds.extend([
                EdgeKind::SqlCalls,
                EdgeKind::QueriesTable,
                EdgeKind::CallsStoredProcedure,
                EdgeKind::StoredProcReadsTable,
                EdgeKind::StoredProcWritesTable,
            ]);
        }
        let mut contract_ids = Vec::new();
        for kind in kinds {
            let (edges, truncated) =
                graph.outgoing_edges_of_kind(project, kind.clone(), &node.node_id, MAX_EDGES)?;
            if truncated {
                result.gaps.push(format!(
                    "{}: {} edges truncated at {MAX_EDGES}",
                    node.node_id,
                    kind.as_str()
                ));
            }
            for edge in edges {
                if edge.generation != node.generation {
                    result.gaps.push(format!(
                        "{} -> {}: edge revision differs from selected source; withheld",
                        node.node_id, edge.target_id
                    ));
                    continue;
                }
                if result.paths.len() >= MAX_EDGES {
                    result.gaps.push(format!(
                        "Complete path response truncated at {MAX_EDGES} edges"
                    ));
                    queue.clear();
                    break;
                }
                let target = graph.get_node(project, &edge.target_id)?;
                let target_verified = match &target {
                    Some(target) => verified_node(graph, project, target, sources, selected)?,
                    None => false,
                };
                if kind == EdgeKind::Calls
                    && !target_verified
                    && let Some(resolver) = &mut control_calls
                    && let Some((target, evidence)) = resolver.resolve(&node, &edge.target_id)?
                {
                    result.paths.push(Hop {
                        from: node.node_id.clone(),
                        to: target.node_id.clone(),
                        kind: "webforms_registered_control_call".into(),
                        target_name: target.name.clone(),
                        target_file: Some(target.file_path.to_string()),
                        target_line: Some(target.start_line),
                        target_source_verified: true,
                        evidence: Some(evidence),
                        table_mapping: None,
                    });
                    queue.push_back((target, depth + 1));
                    continue;
                }
                if kind == EdgeKind::Calls && target_verified
                    && target.as_ref().and_then(|n| n.metadata.as_ref())
                        .and_then(|m| m.get("interface_member_declaration"))
                        .is_some_and(|v| v == "true" || v == true)
                {
                    if contract_ids.len() < 16 {
                        contract_ids.push(edge.target_id.clone());
                    } else {
                        result.gaps.push("Interface implementation expansion limited to 16 called contracts per member".into());
                    }
                }
                let mut table_mapping = None;
                if kind == EdgeKind::QueriesTable
                    && matches!(node.language.as_str(), "vb" | "vbnet")
                    && edge
                        .metadata
                        .as_ref()
                        .and_then(|v| v.get("orm"))
                        .is_some_and(|v| v == "true" || v == true)
                    && let Some(mappings) = &mappings
                    && let Some(source) = sources.iter().find(|s| s.path == node.file_path.as_str())
                {
                    let body = source
                        .content
                        .lines()
                        .skip(node.start_line as usize - 1)
                        .take((node.end_line - node.start_line + 1) as usize)
                        .collect::<Vec<_>>()
                        .join("\n");
                    match mappings.resolve(&node, &body, &edge) {
                        Ok(binding) => table_mapping = Some(binding),
                        Err(reason) if result.gaps.len() < 64 => result
                            .gaps
                            .push(format!("{} -> {}: {reason}", node.node_id, edge.target_id)),
                        Err(_) => {}
                    }
                }
                result.paths.push(Hop {
                    from: node.node_id.clone(),
                    to: edge.target_id.clone(),
                    kind: kind.as_str().into(),
                    target_name: target
                        .as_ref()
                        .map_or_else(|| edge.target_id.clone(), |n| n.name.clone()),
                    target_file: target
                        .as_ref()
                        .filter(|_| target_verified)
                        .map(|n| n.file_path.to_string()),
                    target_line: target
                        .as_ref()
                        .filter(|_| target_verified)
                        .map(|n| n.start_line),
                    target_source_verified: target_verified,
                    evidence: edge.metadata,
                    table_mapping,
                });
                if matches!(
                    kind,
                    EdgeKind::Calls | EdgeKind::CallsStoredProcedure | EdgeKind::SqlCalls
                ) {
                    if let Some(target) = target {
                        queue.push_back((target, depth + 1));
                    } else {
                        result
                            .gaps
                            .push(format!("{}: unresolved target", edge.target_id));
                    }
                }
            }
        }
        if !contract_ids.is_empty() {
            // Reuse the compiler-backed call/Implements join. The callback uses
            // the file already read by that transaction; no nested graph reads.
            let admit = |candidate: &Node, file: Option<&Node>| {
                use engram_graph::store::ImpactAdmission;
                if selected.get(candidate.file_path.as_str()) != Some(&candidate.generation) {
                    return Ok(ImpactAdmission::NonSelectedRevision);
                }
                let current = file.and_then(|file| file.metadata.as_ref())
                    .and_then(|meta| meta.get("file_hash")).and_then(|hash| hash.as_str())
                    .zip(sources.iter().find(|source| source.path == candidate.file_path.as_str()))
                    .is_some_and(|(hash, source)| hash == blake3::hash(source.content.as_bytes()).to_hex().as_str());
                Ok(if current { ImpactAdmission::Selected } else { ImpactAdmission::Unknown })
            };
            let dispatch = graph.bounded_interface_implementations(
                project, &node.node_id, &contract_ids, &admit, &|_| false)?;
            for reason in dispatch.coverage.reasons {
                result.gaps.push(format!("{}: interface implementation coverage: {reason}", node.node_id));
            }
            for route in dispatch.routes {
                if result.paths.len() >= MAX_EDGES {
                    result.gaps.push(format!("Complete path response truncated at {MAX_EDGES} edges"));
                    break;
                }
                let implementation = route.implementation;
                if !verified_node(graph, project, &implementation, sources, selected)? {
                    result.gaps.push(format!("{}: implementation source changed; candidate withheld", implementation.node_id));
                    continue;
                }
                result.paths.push(Hop {
                    from: route.contract.node_id,
                    to: implementation.node_id.clone(),
                    kind: "interface_implementation_candidate".into(),
                    target_name: implementation.name.clone(),
                    target_file: Some(implementation.file_path.to_string()),
                    target_line: Some(implementation.start_line),
                    target_source_verified: true,
                    evidence: Some(serde_json::json!({
                        "caller":route.caller.node_id,"call_lines":route.reference_lines,
                        "call_lines_truncated":route.reference_lines_truncated,
                        "implements_clause_line":route.clause_line,"contract_type":route.contract_type,
                        "contract_type_key":route.contract_type_key,
                        "call_generation":route.call_generation,"mapping_generation":route.mapping_generation,
                        "interpretation":"Compiler-backed explicit Implements candidate for the same called contract. Runtime receiver, DI registration selection and execution are not proven; downstream paths are conditional on this candidate."
                    })),
                    table_mapping: None,
                });
                queue.push_back((implementation, depth + 2));
            }
        }
    }
    result.gaps.sort();
    result.gaps.dedup();
    Ok(result)
}
