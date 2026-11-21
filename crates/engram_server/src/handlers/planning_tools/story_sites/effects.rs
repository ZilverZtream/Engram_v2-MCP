//! Establish the business entity and execution evidence before ranking criterion matches.
use super::{BTreeSet, HashMap, terms};
use engram_graph::{EdgeKind, GraphStore, Node};

#[derive(Default)]
pub(super) struct Domain {
    pub types: BTreeSet<String>,
    tables: BTreeSet<String>,
    activities: BTreeSet<String>,
}

impl Domain {
    pub fn from_types(
        graph: &GraphStore,
        project: &str,
        types: &std::collections::BTreeMap<String, BTreeSet<String>>,
        pool: &mut HashMap<String, usize>,
    ) -> Self {
        let mut domain = Self::default();
        let mut table_counts = HashMap::<String, usize>::new();
        for (name, ids) in types.iter().take(12) {
            let mut ids = ids.iter();
            let Some(id) = ids.next() else { continue };
            if ids.next().is_some() {
                continue;
            }
            let Some(owner) = graph.get_node(project, id).ok().flatten() else {
                continue;
            };
            domain.types.insert(name.to_ascii_lowercase());
            domain
                .activities
                .extend(terms(name).into_iter().filter(|w| {
                    matches!(
                        w.as_str(),
                        "import"
                            | "export"
                            | "transfer"
                            | "sync"
                            | "dispatch"
                            | "upload"
                            | "download"
                            | "publish"
                            | "migration"
                    )
                }));
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

    pub fn competing_activity(&self, node: &Node) -> bool {
        let owner = node.name.rsplit_once('.').map_or("", |(owner, _)| owner);
        let file = std::path::Path::new(node.file_path.as_str())
            .file_name()
            .and_then(|p| p.to_str())
            .unwrap_or("");
        !self
            .activities
            .is_disjoint(&terms(&format!("{owner} {file}")))
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
    execution_effect: bool,
) -> Option<(usize, usize, String)> {
    if execution_effect
        && criterion.contains("execution")
        && ["delete", "deletion", "deletions"]
            .iter()
            .any(|w| criterion.contains(*w))
    {
        for (i, (number, text)) in lines.iter().enumerate() {
            let lower = text.to_ascii_lowercase();
            if [".deletebyid(", ".deleteonsubmit(", "delete from "]
                .iter()
                .any(|w| lower.contains(w))
                && lines[i.saturating_sub(4)..i].iter().any(|(_, l)| {
                    let s = l.to_ascii_lowercase();
                    (s.contains("if ") || s.contains("case ")) && s.contains("delete")
                })
            {
                return Some((280, *number, text.clone()));
            }
        }
    }
    if !writes {
        return None;
    }
    let has = |words: &[&str]| words.iter().any(|w| criterion.contains(*w));
    if let Some((line, text)) = transaction_step(lines, criterion) {
        return Some((230, line, text.to_owned()));
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
        && let Some((line, text)) = lines.iter().find(|(_, l)| {
            let Some((property, _)) = l.split_once(" = ") else {
                return false;
            };
            property.contains('.')
                && !property.contains(char::is_whitespace)
                && terms(property).intersection(criterion).count() >= 2
        })
    {
        return Some((300, *line, text.clone()));
    }
    None
}

fn transaction_step<'a>(
    lines: &'a [(usize, String)],
    criterion: &BTreeSet<String>,
) -> Option<(usize, &'a str)> {
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
        for (offset, (line, text)) in lines[start..commit].iter().enumerate() {
            let words = terms(text);
            let requested = [
                "move", "transfer", "copy", "merge", "publish", "archive", "delete",
            ]
            .iter()
            .any(|op| criterion.contains(*op) && words.contains(*op));
            if requested
                && (words.intersection(criterion).count() >= 2
                    || lines[(start + offset).saturating_sub(2)..start + offset]
                        .iter()
                        .any(|(_, l)| terms(l).intersection(criterion).count() >= 2))
                && text.contains('(')
                && !text.trim_start().to_lowercase().starts_with("if ")
            {
                return Some((*line, text.as_str()));
            }
        }
    }
    None
}

/// Attribute a selected edit owner using the operation it performs. Coverage of other
/// criteria influences which sites are selected, but does not change the meaning of a call.
pub(super) fn attribution<'a>(
    lines: &'a [(usize, String)],
    criterion: &BTreeSet<String>,
) -> Option<(usize, usize, &'a str)> {
    let mut best = transaction_step(lines, criterion).map(|(line, text)| {
        let context = terms(text).intersection(criterion).count();
        let execution = ["before", "execution", "recheck", "validate"]
            .iter()
            .any(|w| criterion.contains(*w));
        (20 + 3 * context + 12 * usize::from(execution), line, text)
    });
    for (line, text) in lines {
        if !text.contains(',') || !super::durable_write(text) {
            continue;
        }
        let Some((_, args)) = text.split_once('(') else {
            continue;
        };
        // Count distinct named payload arguments, excluding expressions and literals.
        let fields: BTreeSet<_> = args
            .split([',', ')'])
            .map(str::trim)
            .filter(|v| {
                v.len() >= 3
                    && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                    && !["true", "false", "null", "nothing", "none"]
                        .contains(&v.to_ascii_lowercase().as_str())
            })
            .map(|v| super::term(&v.to_ascii_lowercase()))
            .filter(|v| criterion.contains(v))
            .collect();
        if fields.len() >= 3 {
            let evidence = (40 + fields.len(), *line, text.as_str());
            if best.as_ref().is_none_or(|b| evidence.0 > b.0) {
                best = Some(evidence);
            }
        }
    }
    best
}
