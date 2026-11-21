//! Establish the business entity and execution evidence before ranking criterion matches.
use super::{BTreeSet, HashMap, terms};
use engram_graph::{EdgeKind, GraphStore, Node};

#[derive(Default)]
pub(super) struct Domain {
    pub types: BTreeSet<String>,
    tables: BTreeSet<String>,
}

impl Domain {
    pub fn from_types(
        graph: &GraphStore,
        project: &str,
        types: &BTreeSet<String>,
        pool: &mut HashMap<String, usize>,
    ) -> Self {
        let mut domain = Self::default();
        let mut table_counts = HashMap::<String, usize>::new();
        for name in types.iter().take(12) {
            let found = graph
                .query_nodes_by_symbol_name(project, name, None, 16)
                .unwrap_or_default();
            let declarations: Vec<_> = found
                .iter()
                .filter(|n| matches!(n.node_type.as_str(), "class" | "struct" | "module"))
                .collect();
            let [owner] = declarations.as_slice() else {
                continue;
            };
            domain.types.insert(name.clone());
            for method in graph
                .functions_for_source_lines(project, owner.file_path.as_str())
                .unwrap_or_default()
                .into_iter()
                .filter(|n| n.start_line >= owner.start_line && n.end_line <= owner.end_line)
                .take(96)
            {
                pool.entry(method.node_id.clone()).or_insert(30);
                if let Ok(tables) =
                    graph.neighbors(project, EdgeKind::QueriesTable, &method.node_id, 32)
                {
                    for (id, _) in tables {
                        *table_counts.entry(id).or_default() += 1;
                    }
                }
            }
        }
        domain.tables.extend(
            table_counts
                .into_iter()
                .filter(|(_, count)| *count >= 2)
                .map(|(id, _)| id),
        );
        domain
    }

    pub fn matches(
        &self,
        graph: &GraphStore,
        project: &str,
        node: &Node,
        lines: &[(usize, String)],
    ) -> bool {
        if self.types.is_empty() {
            return false;
        }
        let typed = |s: &str| {
            s.split(|c: char| !c.is_alphanumeric() && c != '_')
                .any(|id| self.types.contains(&id.to_ascii_lowercase()))
        };
        typed(&node.name)
            || lines.iter().any(|(_, l)| typed(l))
            || (!self.tables.is_empty()
                && graph
                    .neighbors(project, EdgeKind::QueriesTable, &node.node_id, 48)
                    .is_ok_and(|tables| tables.iter().any(|(id, _)| self.tables.contains(id))))
    }
}

pub(super) fn requires_effect(criterion: &BTreeSet<String>) -> bool {
    [
        "refuse",
        "refused",
        "refusal",
        "enforce",
        "enforces",
        "validate",
        "validates",
        "unauthorized",
        "recheck",
    ]
    .iter()
    .any(|w| criterion.contains(*w))
        || (criterion.contains("check")
            && ["before", "execution", "permission", "access"]
                .iter()
                .any(|w| criterion.contains(*w)))
}

pub(super) fn guard(lines: &[(usize, String)]) -> bool {
    lines.iter().any(|(_, line)| {
        let lower = line.to_ascii_lowercase();
        (lower.starts_with("if ")
            || lower.starts_with("if(")
            || lower.contains("throw ")
            || lower.contains("guard.")
            || lower.contains("authorize(")
            || lower.contains("ensure"))
            && [
                "access",
                "permit",
                "permission",
                "authoriz",
                "denied",
                "forbidden",
                "refus",
                "restrict",
                "cannot",
            ]
            .iter()
            .any(|s| lower.contains(s))
    })
}

/// Prefer an executable line that identifies an uncovered part of the requested change.
/// A transaction's move pass, a destructive replacement or a persisted repair supplies
/// stronger evidence than an event or helper whose name happens to share the same verb.
pub(super) fn workflow(
    lines: &[(usize, String)],
    method: &BTreeSet<String>,
    criterion: &BTreeSet<String>,
    writes: bool,
) -> Option<(usize, usize, String)> {
    if !writes {
        return None;
    }
    let has = |words: &[&str]| words.iter().any(|w| criterion.contains(*w));
    // A named command inside a committed batch identifies the execution site even
    // when its enclosing method is named for the pipeline rather than the command.
    if let Some(commit) = lines
        .iter()
        .rposition(|(_, l)| l.to_ascii_lowercase().contains(".commit("))
    {
        let start = lines[..commit]
            .iter()
            .rposition(|(_, l)| {
                let s = l.to_ascii_lowercase();
                s.contains("begintransaction(") || s.contains("begin_transaction(")
            })
            .unwrap_or(commit.saturating_sub(12));
        for (line, text) in &lines[start..commit] {
            let words = terms(text);
            let requested = [
                "move", "transfer", "copy", "merge", "publish", "archive", "delete",
            ]
            .iter()
            .any(|op| criterion.contains(*op) && words.contains(*op));
            if requested
                && words.intersection(criterion).count() >= 2
                && text.contains('(')
                && !text.trim_start().to_lowercase().starts_with("if ")
            {
                return Some((230, *line, text.clone()));
            }
        }
    }
    if has(&["replacement", "replace", "replacing"])
        && has(&["deleting", "delete", "deletion"])
        && !method.is_disjoint(criterion)
        && ["replace", "change", "convert"]
            .iter()
            .any(|w| method.contains(*w))
        && lines.iter().any(|(_, l)| {
            let t = l.to_ascii_lowercase();
            t.contains(".writetoserver(")
                || t.contains(".insertonsubmit(")
                || t.contains("insert into ")
        })
        && let Some((line, text)) = lines.iter().find(|(_, l)| {
            let t = l.to_ascii_lowercase();
            t.contains("delete from ") || t.contains(".deleteallonsubmit(")
        })
    {
        return Some((205, *line, text.clone()));
    }
    if has(&["repair", "correction", "correct"])
        && ["repair", "fix", "correct"]
            .iter()
            .any(|w| method.contains(*w))
        && method.intersection(criterion).count() >= 2
        && let Some((line, text)) = lines
            .iter()
            .find(|(_, l)| l.contains(" = ") && terms(l).intersection(criterion).count() >= 2)
    {
        return Some((175, *line, text.clone()));
    }
    None
}
