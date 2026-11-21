//! Bounded caller evidence from the same worktree used for a VB review.
use super::{DiffFile, GateContext, function_spans, review_executable_text};
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

struct Method {
    file: String,
    owner: String,
    name: String,
    start: usize,
    end: usize,
    calls: Vec<(usize, String)>,
    audits: Vec<usize>,
    blocks: Vec<usize>,
}
impl Method {
    fn same_block(&self, first: usize, later: usize) -> bool {
        self.blocks.get(first - self.start) == self.blocks.get(later - self.start)
    }
}
pub(super) struct Sites {
    methods: Vec<Method>,
    source: BTreeMap<String, Vec<String>>,
}
fn calls_target(caller: &Method, name: &str, target: &Method) -> bool {
    if name == target.name {
        return caller.file == target.file;
    }
    let owner_method = format!("{}.{}", target.owner, target.name);
    name == owner_method || name.ends_with(&format!(".{owner_method}"))
}
impl Sites {
    pub(super) fn from_review(ctx: &GateContext<'_>, audit: &str) -> Self {
        let mut files: BTreeSet<_> = ctx
            .diff_files
            .iter()
            .filter(|df| df.path.to_ascii_lowercase().ends_with(".vb"))
            .map(|df| df.path.clone())
            .collect();
        // An indexed caller can sit outside the changed set. Only its current
        // worktree text can establish the qualified call and audit sequence.
        let mut frontier = Vec::new();
        for df in ctx
            .diff_files
            .iter()
            .filter(|df| df.path.to_ascii_lowercase().ends_with(".vb"))
        {
            if super::audit_mutations(df).is_empty() {
                continue;
            }
            if let Ok(nodes) =
                ctx.graph
                    .query_nodes_in_file(ctx.project_id, Some("function"), &df.path, 500)
            {
                frontier.extend(
                    nodes
                        .into_iter()
                        .filter(|n| {
                            n.node_type == "function"
                                && df.added_lines.iter().any(|(i, _)| {
                                    *i >= n.start_line as usize && *i <= n.end_line as usize
                                })
                        })
                        .map(|n| n.node_id),
                );
            }
        }
        let mut visited = BTreeSet::new();
        for _ in 0..2 {
            let mut next = Vec::new();
            for id in frontier.drain(..).take(64) {
                if !visited.insert(id.clone()) {
                    continue;
                }
                if let Ok(incoming) = ctx.graph.find_incoming_edges_with_kind(
                    ctx.project_id,
                    Some(engram_graph::EdgeKind::Calls),
                    &id,
                    24,
                ) {
                    for (caller, _, _) in incoming {
                        if let Ok(Some(node)) = ctx.graph.get_node(ctx.project_id, &caller) {
                            files.insert(node.file_path.as_str().to_owned());
                            next.push(caller);
                        }
                    }
                }
            }
            frontier = next;
        }
        static CALL: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"(?i)\b([a-z_]\w*(?:\.[a-z_]\w*)*)\s*\(").unwrap());
        static LOG: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(
                r"(?i)(?:^|\.)(?:\w*(?:audit|logg?|trail)\w*)\.(?:create|write|record|append|log)$",
            )
            .unwrap()
        });
        let mut methods = Vec::new();
        let mut source = BTreeMap::new();
        let audit = audit.to_ascii_lowercase();
        for file in files {
            let Ok(text) = engram_core::safe_read_to_string(ctx.project_dir, &file) else {
                continue;
            };
            let owner = std::path::Path::new(&file)
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_ascii_lowercase();
            let lines: Vec<_> = text.lines().map(str::to_owned).collect();
            for (start, end, name) in function_spans(&text, true) {
                let mut calls = Vec::new();
                let mut audits = Vec::new();
                // Keep evidence on the calling branch. A log in the failure
                // arm of a null check does not witness a successful mutation.
                let mut stack = vec![start];
                let mut blocks = vec![start];
                for (i, line) in lines.iter().enumerate().take(end).skip(start) {
                    let masked = review_executable_text(line, true, true);
                    let lower = masked.trim().to_ascii_lowercase();
                    let first = lower.split_whitespace().next().unwrap_or("");
                    let end_block = matches!(
                        lower.as_str(),
                        "end if" | "end try" | "end while" | "end using" | "end select"
                    ) || matches!(first, "next" | "loop");
                    let sibling = matches!(first, "else" | "elseif" | "catch" | "finally");
                    if (end_block || sibling) && stack.len() > 1 {
                        stack.pop();
                    }
                    let opens = (first == "if" && lower.ends_with(" then"))
                        || matches!(first, "for" | "do" | "while" | "try" | "using" | "select");
                    if opens || sibling {
                        stack.push(i + 1);
                    }
                    blocks.push(*stack.last().unwrap());

                    for m in CALL.captures_iter(&masked) {
                        let target = m[1].to_ascii_lowercase();
                        if target == audit || LOG.is_match(&target) {
                            audits.push(i + 1);
                        }
                        calls.push((i + 1, target));
                    }
                }
                methods.push(Method {
                    file: file.clone(),
                    owner: owner.clone(),
                    name: name.to_ascii_lowercase(),
                    start,
                    end,
                    calls,
                    audits,
                    blocks,
                });
            }
            source.insert(file, lines);
        }
        Self { methods, source }
    }
    pub(super) fn witness(&self, df: &DiffFile, line: usize) -> Option<String> {
        let current = self.source.get(&df.path)?.get(line.checked_sub(1)?)?;
        if !df
            .added_lines
            .iter()
            .any(|(i, l)| *i == line && l.trim() == current.trim())
        {
            return None;
        }
        let target = self
            .methods
            .iter()
            .find(|m| m.file == df.path && (m.start..=m.end).contains(&line))?;
        if let Some(at) = target
            .audits
            .iter()
            .find(|at| **at >= line && target.same_block(line, **at))
        {
            return Some(format!("{}:{line} -> audit at {}:{at}", df.path, df.path));
        }
        let mut frontier = vec![(target, format!("{}:{}", df.path, line))];
        for _ in 0..2 {
            let mut next = Vec::new();
            for (target, chain) in frontier {
                for caller in &self.methods {
                    let Some((at, _)) = caller
                        .calls
                        .iter()
                        .find(|(_, n)| calls_target(caller, n, target))
                    else {
                        continue;
                    };
                    let path = format!("{chain} <- {}:{at}", caller.file);
                    if let Some(log) = caller
                        .audits
                        .iter()
                        .find(|n| *n > at && caller.same_block(*at, **n))
                    {
                        return Some(format!("{path} -> audit at {}:{log}", caller.file));
                    }
                    for (at, name) in caller
                        .calls
                        .iter()
                        .filter(|(n, _)| n > at && caller.same_block(*at, *n))
                    {
                        if let Some(helper) = self
                            .methods
                            .iter()
                            .find(|m| !m.audits.is_empty() && calls_target(caller, name, m))
                        {
                            return Some(format!(
                                "{path} -> {}:{at} -> audit at {}:{}",
                                caller.file, helper.file, helper.audits[0]
                            ));
                        }
                    }
                    next.push((caller, path));
                }
            }
            frontier = next;
        }
        None
    }
}
