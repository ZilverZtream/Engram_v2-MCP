//! Select implementation evidence while the planner already visits symbol names.
use super::{canonical_story_name_term, name_tokens, split_symmetric_name_tokens};
use engram_graph::{EdgeKind, GraphStore, Node};
use std::{
    cmp::Reverse,
    collections::{BTreeSet, HashMap},
    path::Path,
    sync::LazyLock,
};

fn term(word: &str) -> String {
    match word {
        "configuration" | "config" | "setup" | "settings" => "setting".into(),
        "synchronization" | "synchronisation" | "synchronize" => "sync".into(),
        "saving" | "saved" => "save".into(),
        "copying" | "copied" => "copy".into(),
        "deleting" | "deleted" => "delete".into(),
        "changing" | "changed" => "change".into(),
        "moving" | "moved" => "move".into(),
        "importing" => "import".into(),
        _ => canonical_story_name_term(word),
    }
}
fn terms(text: &str) -> BTreeSet<String> {
    split_symmetric_name_tokens(text)
        .into_iter()
        .filter(|w| {
            w.len() >= 3
                && !matches!(
                    w.as_str(),
                    "the"
                        | "and"
                        | "from"
                        | "with"
                        | "between"
                        | "when"
                        | "must"
                        | "should"
                        | "this"
                        | "that"
                        | "user"
                        | "into"
                )
        })
        .map(|w| term(&w))
        .collect()
}

struct Criterion {
    label: String,
    terms: BTreeSet<String>,
}
pub(super) struct Candidates {
    title: BTreeSet<String>,
    criteria: Vec<Criterion>,
    selected: BTreeSet<(Reverse<usize>, String)>,
    owner: String,
    owner_terms: BTreeSet<String>,
}
impl Candidates {
    pub(super) fn new(scope: &str) -> Self {
        let heading = scope
            .lines()
            .next()
            .unwrap_or("")
            .trim_start_matches('#')
            .trim();
        let heading = heading.split_once(": ").map_or(heading, |(_, title)| title);
        let title = terms(heading.split(" must ").next().unwrap_or(heading));
        let mut criteria = Vec::new();
        let mut in_criteria = false;
        let explicit_section = scope.lines().any(|l| {
            l.trim()
                .trim_start_matches('#')
                .trim()
                .trim_end_matches(':')
                .eq_ignore_ascii_case("acceptance criteria")
        });
        for line in scope.lines() {
            let trimmed = line.trim();
            let heading = trimmed.trim_start_matches('#').trim().trim_end_matches(':');
            if heading.eq_ignore_ascii_case("acceptance criteria") {
                in_criteria = true;
                continue;
            }
            if trimmed.starts_with('#') && in_criteria {
                break;
            }
            if explicit_section && !in_criteria {
                continue;
            }
            if heading.eq_ignore_ascii_case("comments")
                || heading.to_ascii_lowercase().starts_with("edit history")
            {
                break;
            }
            let item = trimmed.trim_start_matches(['-', '*']).trim();
            let id = item.strip_prefix("AC").unwrap_or(item);
            let numbered = id.split_once(['.', ')', ':']).filter(|(n, _)| {
                !n.is_empty() && n.len() <= 3 && n.bytes().all(|b| b.is_ascii_digit())
            });
            let (label, body) = match numbered {
                Some((n, body)) => (format!("AC{n}"), body),
                None if in_criteria && item != trimmed => {
                    (format!("Criterion {}", criteria.len() + 1), item)
                }
                _ => continue,
            };
            let words = terms(&super::planning_scope(body));
            if !words.is_empty() {
                criteria.push(Criterion {
                    label,
                    terms: words,
                });
            }
            if criteria.len() == 24 {
                break;
            }
        }
        if criteria.is_empty() {
            criteria.push(Criterion {
                label: "Story".into(),
                terms: terms(
                    &super::planning_scope(scope)
                        .chars()
                        .take(2400)
                        .collect::<String>(),
                ),
            });
        }
        Self {
            title,
            criteria,
            selected: BTreeSet::new(),
            owner: String::new(),
            owner_terms: BTreeSet::new(),
        }
    }
    pub(super) fn include(&mut self, id: &str) {
        self.selected.insert((Reverse(1), id.into()));
    }
    pub(super) fn consider(&mut self, name: &str, id: &str, path: &str, words: &[String]) {
        if words.is_empty() {
            return;
        }
        let owner = name
            .rsplit_once(['.', ':'])
            .map_or("", |(parent, _)| parent);
        // Node identities are ordered by file. Keep the file/owner terms for its methods.
        let key = format!("{path}|{owner}");
        if self.owner != key {
            self.owner = key;
            self.owner_terms = terms(owner);
            self.owner_terms.extend(terms(
                path.rsplit('/')
                    .next()
                    .unwrap_or(path)
                    .split('.')
                    .next()
                    .unwrap_or(""),
            ));
        }
        let leaf: BTreeSet<_> = words.iter().map(|w| term(w)).collect();
        let title_hits = self
            .title
            .iter()
            .filter(|t| leaf.contains(*t) || self.owner_terms.contains(*t))
            .count();
        if title_hits == 0 {
            return;
        }
        let lexical = self
            .criteria
            .iter()
            .map(|c| c.terms.intersection(&leaf).count())
            .max()
            .unwrap_or(0);
        let in_leaf = self.title.intersection(&leaf).count();
        if (title_hits < 2 && in_leaf == 0) || (in_leaf + lexical == 0) {
            return;
        }
        let score = 4 * title_hits + 3 * in_leaf + 2 * lexical;
        self.selected.insert((Reverse(score), id.to_owned()));
        if self.selected.len() > 12 {
            self.selected.pop_last();
        }
    }

    pub(super) fn rank(&self, graph: &GraphStore, project: &str, root: &Path) -> Vec<Site> {
        let mut source = Source {
            graph,
            project,
            root,
            files: HashMap::new(),
        };
        let mut sites = Vec::new();
        for (Reverse(lexical), id) in &self.selected {
            let Ok(Some(node)) = graph.get_node(project, id) else {
                continue;
            };
            let Some(lines) = source.lines(&node) else {
                continue;
            };
            let (mut best, mut criterion, mut observations) = (0, String::new(), Vec::new());
            for c in &self.criteria {
                let mut scored = Vec::new();
                for (line, text) in &lines {
                    let weight = line_score(text, &c.terms, &self.title);
                    if weight > 0 {
                        scored.push((Reverse(weight), *line, text));
                    }
                }
                scored.sort();
                let quality = scored.first().map_or(0, |r| r.0.0);
                if quality > best {
                    best = quality;
                    criterion = c.label.clone();
                    observations = scored
                        .into_iter()
                        .take(3)
                        .map(|(_, n, s)| (n, s.to_string()))
                        .collect();
                }
            }
            if observations.is_empty() {
                continue;
            }
            sites.push((Reverse(*lexical + best), node, criterion, observations));
        }
        sites.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.node_id.cmp(&b.1.node_id)));
        sites
            .into_iter()
            .take(3)
            .map(|(_, node, label, lines)| {
                let path = node.file_path.as_str().to_owned();
                let mut evidence: Vec<_> = lines
                    .iter()
                    .map(|(n, s)| format!("{path}:{n}  {s}"))
                    .collect();
                let mut paths = vec![path];
                // Only follow a write actually shown in the selected source. A thin
                // wrapper may delegate once more before touching persistent storage.
                if let Some(call) = lines.iter().find_map(|(_, s)| write_call(s)) {
                    source.follow_write(&node, &call, 2, &mut evidence, &mut paths);
                }
                Site {
                    name: node.name,
                    label,
                    lines: evidence,
                    paths,
                }
            })
            .collect()
    }
}
pub(super) struct Site {
    pub name: String,
    pub label: String,
    pub lines: Vec<String>,
    pub paths: Vec<String>,
}

static CALL: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)\b([a-z_][a-z0-9_]*)\s*\(").unwrap());
fn write_call(text: &str) -> Option<String> {
    CALL.captures_iter(text).find_map(|c| {
        let name = &c[1];
        let tokens = name_tokens(name);
        let first = tokens.first()?.as_str();
        (matches!(
            first,
            "set"
                | "save"
                | "write"
                | "insert"
                | "update"
                | "delete"
                | "remove"
                | "persist"
                | "upsert"
                | "commit"
                | "submit"
        ) || matches!(
            name.to_ascii_lowercase().as_str(),
            "executenonquery" | "writetoserver"
        ))
        .then(|| name.to_owned())
    })
}
fn line_score(text: &str, criterion: &BTreeSet<String>, title: &BTreeSet<String>) -> usize {
    let words = terms(text);
    let matching = words.intersection(criterion).count();
    let subject = words.intersection(title).count();
    let write = write_call(text).is_some();
    if !write && matching + subject < 2 {
        return 0;
    }
    let conditional = text.to_ascii_lowercase().starts_with("if ");
    let evidence = usize::from(write) * 8 + usize::from(conditional) * 2;
    evidence + matching.min(6) * 3 + subject.min(4) * 2
}
struct Source<'a> {
    graph: &'a GraphStore,
    project: &'a str,
    root: &'a Path,
    files: HashMap<String, Option<Vec<String>>>,
}
impl Source<'_> {
    fn lines(&mut self, node: &Node) -> Option<Vec<(usize, String)>> {
        let path = node.file_path.as_str();
        let file = self
            .files
            .entry(path.into())
            .or_insert_with(|| {
                crate::planner_source::read(self.graph, self.project, self.root, path)
                    .ok()
                    .map(|text| text.lines().map(str::to_owned).collect())
            })
            .as_ref()?;
        let start = node.start_line.checked_sub(1)? as usize;
        let end = (node.end_line as usize).min(start + 240);
        // Omit declarations, comments and terminators from source witnesses.
        Some(
            file.get(start + 1..end)?
                .iter()
                .enumerate()
                .filter_map(|(i, s)| {
                    let s = s.trim();
                    (!s.is_empty() && !s.starts_with(['\'', '/', '*', '#']) && s.len() <= 420)
                        .then(|| (start + i + 2, s.to_owned()))
                })
                .collect(),
        )
    }
    fn follow_write(
        &mut self,
        node: &Node,
        call: &str,
        depth: usize,
        out: &mut Vec<String>,
        paths: &mut Vec<String>,
    ) {
        let Ok(edges) = self
            .graph
            .neighbors(self.project, EdgeKind::Calls, &node.node_id, 64)
        else {
            return;
        };
        let mut targets = Vec::new();
        for (id, _) in edges {
            if !id.to_ascii_lowercase().contains(&call.to_ascii_lowercase()) {
                continue;
            }
            if let Ok(Some(n)) = self.graph.get_node(self.project, &id) {
                if n.node_type == "function"
                    && n.name
                        .rsplit(['.', ':'])
                        .next()
                        .is_some_and(|s| s.eq_ignore_ascii_case(call))
                {
                    targets.push(n);
                }
            }
        }
        let mut binding = "indexed callee";
        if targets.is_empty() {
            let Ok(local) = self.graph.query_nodes_by_symbol_name(
                self.project,
                call,
                Some(node.file_path.as_str()),
                3,
            ) else {
                return;
            };
            targets = local
                .into_iter()
                .filter(|n| {
                    n.file_path == node.file_path
                        && n.node_type == "function"
                        && n.name
                            .rsplit(['.', ':'])
                            .next()
                            .is_some_and(|name| name.eq_ignore_ascii_case(call))
                })
                .collect();
            binding = "same-file candidate";
        }
        let [target] = targets.as_slice() else { return };
        let Some(lines) = self.lines(target) else {
            return;
        };
        // Prefer an actual storage mutation to a generic commit or another wrapper.
        let found = lines
            .iter()
            .filter_map(|(n, s)| {
                let callee = write_call(s)?;
                let lower = callee.to_lowercase();
                let score = if ["insert", "delete", "update", "write", "upsert"]
                    .iter()
                    .any(|p| lower.starts_with(p))
                {
                    3
                } else if lower.starts_with("set") || lower.starts_with("save") {
                    2
                } else {
                    1
                };
                Some((Reverse(score), *n, s, callee))
            })
            .min();
        if let Some((_, n, s, next)) = found {
            let path = target.file_path.as_str();
            out.push(format!("{path}:{n}  {s} ({binding} {})", target.name));
            paths.push(path.into());
            if depth > 1 && target.node_id != node.node_id {
                self.follow_write(target, &next, depth - 1, out, paths);
            }
        }
    }
}
