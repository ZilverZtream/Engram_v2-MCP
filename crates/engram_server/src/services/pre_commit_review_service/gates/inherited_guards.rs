//! Source-bound base-class conventions are review evidence, never authorization.
use super::{GateContext, RE_GP_NEW_ENDPOINT, ReviewFinding, Severity, guard_names_in};
use crate::services::pre_commit_review_service::DiffFile;
use engram_graph::{EdgeKind, Node};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io::Read,
};

const TYPE_LIMIT: usize = 16;
const FILE_LIMIT: usize = 256 * 1024;

/// Only unchanged, selected source revisions may contribute inherited evidence.
/// The review is still useful when the watcher has not caught up: that provider
/// is explicitly degraded rather than trusting stale inheritance or base bodies.
fn current_source(ctx: &GateContext<'_>, node: &Node) -> anyhow::Result<String> {
    anyhow::ensure!(
        node.namespace == engram_core::namespaces::NAMESPACE_MEMORY
            && node.generation > 0
            && node.generation <= ctx.generation,
        "type is outside the selected source generation"
    );
    let files =
        ctx.graph
            .query_nodes_in_file(ctx.project_id, Some("file"), node.file_path.as_str(), 2)?;
    anyhow::ensure!(
        files.len() == 1,
        "current file identity is unavailable or ambiguous"
    );
    let file = &files[0];
    anyhow::ensure!(
        file.namespace == node.namespace && file.generation == node.generation,
        "type and file revisions differ"
    );
    let expected = file
        .metadata
        .as_ref()
        .and_then(|value| value.get("file_hash"))
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow::anyhow!("indexed file hash is unavailable"))?;
    let source = engram_core::safe_open_read(ctx.project_dir, node.file_path.as_str())?;
    anyhow::ensure!(
        source.metadata()?.len() <= FILE_LIMIT as u64,
        "base source exceeds the256KiB read limit"
    );
    let mut bytes = Vec::new();
    source.take(FILE_LIMIT as u64 + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= FILE_LIMIT && blake3::hash(&bytes).to_hex().as_str() == expected,
        "working-tree source differs from its indexed revision"
    );
    Ok(String::from_utf8(bytes)?)
}

/// Keep call positions while removing VB strings and apostrophe comments.
fn vb_code(line: &str) -> String {
    let mut out = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '"' {
            if quoted && chars.peek() == Some(&'"') {
                chars.next();
            } else {
                quoted = !quoted;
            }
            out.push(' ');
        } else if !quoted && ch == '\'' {
            break;
        } else {
            out.push(if quoted { ' ' } else { ch });
        }
    }
    out
}

pub(super) fn collect(ctx: &GateContext<'_>, changed: &DiffFile) -> anyhow::Result<Vec<String>> {
    // Start with the VB/WebForms workflow. Do not infer bases by same-name search.
    if !changed.path.to_ascii_lowercase().ends_with(".vb") {
        return Ok(Vec::new());
    }
    let roots = ctx.graph.query_nodes_in_file(
        ctx.project_id,
        Some("class"),
        &changed.path,
        TYPE_LIMIT + 1,
    )?;
    if roots.len() > TYPE_LIMIT {
        ctx.note_cap("Inherited guard review limited to16 types per changed file");
    }
    let endpoint_lines: Vec<_> = changed
        .added_lines
        .iter()
        .filter(|(_, line)| RE_GP_NEW_ENDPOINT.is_match(line))
        .collect();
    let mut queue: VecDeque<_> = roots
        .into_iter()
        .take(TYPE_LIMIT)
        .filter(|node| {
            endpoint_lines.iter().any(|(line, _)| {
                *line >= node.start_line as usize && *line <= node.end_line as usize
            })
        })
        .map(|node| (node, false))
        .collect();
    let mut visited = BTreeSet::new();
    let mut source_cache = BTreeMap::<String, (u64, String)>::new();
    let mut evidence = BTreeSet::new();
    while let Some((node, inherited)) = queue.pop_front() {
        if visited.contains(&node.node_id) {
            continue;
        }
        if visited.len() == TYPE_LIMIT {
            ctx.note_cap("Inherited guard review stopped at16 distinct types; remaining bases are uninspected");
            break;
        }
        visited.insert(node.node_id.clone());
        if node.namespace != engram_core::namespaces::NAMESPACE_MEMORY
            || node.generation == 0
            || node.generation > ctx.generation
        {
            ctx.degrade(format!(
                "{}: inherited guard type {} is outside the selected source generation",
                changed.path, node.name
            ));
            continue;
        }
        let (edges, truncated) = ctx.graph.outgoing_edges_of_kind(
            ctx.project_id,
            EdgeKind::InheritsFrom,
            &node.node_id,
            TYPE_LIMIT,
        )?;
        if !inherited && edges.is_empty() && !truncated {
            continue;
        }
        if truncated {
            ctx.note_cap("Inherited guard review truncated base edges at16 per type");
        }
        let path = node.file_path.as_str();
        let source = match source_cache.get(path) {
            Some((generation, content)) if *generation == node.generation => content.clone(),
            _ => match current_source(ctx, &node) {
                Ok(source) => {
                    source_cache.insert(path.to_owned(), (node.generation, source.clone()));
                    source
                }
                Err(error) => {
                    ctx.degrade(format!(
                        "{}: inherited guard evidence withheld for {}: {error}",
                        changed.path, node.name
                    ));
                    continue;
                }
            },
        };
        if !inherited
            && !endpoint_lines.iter().any(|(line, text)| {
                *line >= node.start_line as usize
                    && *line <= node.end_line as usize
                    && source.lines().nth(line.saturating_sub(1)) == Some(text.as_str())
            })
        {
            ctx.degrade(format!("{}: added handler does not match the current indexed class source; inherited evidence withheld", changed.path));
            continue;
        }
        if inherited {
            let hash = blake3::hash(source.as_bytes()).to_hex().to_string();
            for (offset, line) in source
                .lines()
                .enumerate()
                .skip(node.start_line.saturating_sub(1) as usize)
                .take(node.end_line.saturating_sub(node.start_line) as usize + 1)
            {
                let code = vb_code(line);
                let declaration = super::added_line_fn_def(path, &code);
                let guards: Vec<_> = guard_names_in(&code)
                    .into_iter()
                    .filter(|name| {
                        declaration
                            .as_ref()
                            .is_none_or(|decl| !name.eq_ignore_ascii_case(decl))
                    })
                    .collect();
                if !guards.is_empty() {
                    evidence.insert(format!("inherited guard convention: {} at {path}:{} in {}; source_blake3={hash}; selected_type_generation={}. Lexical call evidence only; applicability and execution for the new handler are unproven.", guards.join(", "), offset + 1, node.name, node.generation));
                }
                if evidence.len() == 16 {
                    break;
                }
            }
            if evidence.len() == 16 {
                ctx.note_cap("Inherited guard evidence limited to16 cited call sites; other base calls are uninspected");
                break;
            }
        }
        for edge in edges {
            if edge.namespace != node.namespace || edge.generation != node.generation {
                ctx.degrade(format!(
                    "{}: inherited guard edge for {} differs from its selected type revision",
                    changed.path, node.name
                ));
                continue;
            }
            match ctx.graph.get_node(ctx.project_id, &edge.target_id)? {
                Some(base) if base.node_type == "class" => queue.push_back((base, true)),
                _ => ctx.degrade(format!(
                    "{}: inherited guard base {} is unresolved; no name-based binding inferred",
                    changed.path, edge.target_id
                )),
            }
        }
    }
    Ok(evidence.into_iter().collect())
}

pub(super) fn review_finding(changed: &DiffFile, evidence: &[String]) -> ReviewFinding {
    ReviewFinding::new(Severity::Info, "guard_parity", &changed.path,
        "New handler has inherited permission conventions to verify",
        "No local permission-call convention was found in this file. Source-verified base classes contain the cited guard calls. Inheritance alone does not establish whether a base lifecycle method or a conditional guard protects this new entry point.",
        "Inspect the cited base methods and this handler's lifecycle/call path. Preserve the inherited authorization contract; add a local guard only where the source and applicable policy require it.")
        .with_evidence(evidence.to_vec())
        .with_next_tool(format!("map_guards_and_settings(scope={}) and get_full_method_body for the cited base source locations", serde_json::to_string(&changed.path).expect("path serialization")))
}
