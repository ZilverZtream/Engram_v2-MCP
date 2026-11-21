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
        "maximum" => "max".into(),
        "minimum" => "min".into(),
        "opening" | "opened" => "open".into(),
        "editing" => "edit".into(),
        "enabled" | "enabling" => "enable".into(),
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
pub(super) fn terms(text: &str) -> BTreeSet<String> {
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
    named_symbols: BTreeSet<String>,
    routes: BTreeSet<String>,
    terms: BTreeSet<String>,
}
pub(super) struct Candidates {
    pub(super) source_evidence: HashMap<String, super::story_sources::Witness>,
    pub(super) history: super::story_history::Evidence,
    title: BTreeSet<String>,
    criteria: Vec<Criterion>,
    operations: Vec<BTreeSet<String>>,
    subject: BTreeSet<String>,
    main_noun: Option<String>,
    operation_candidates: Vec<BTreeSet<(Reverse<usize>, String)>>,
    excluded: BTreeSet<String>,
    narrative: String,
    invoked_names: HashMap<String, usize>,
    invoked: BTreeSet<(Reverse<usize>, String)>,
    writes: bool,
    transport: BTreeSet<(Reverse<usize>, String)>,
    server: BTreeSet<(Reverse<usize>, String)>,
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
                    routes: super::story_routes::mentions(body)
                        .into_iter()
                        .map(|(_, p)| p)
                        .collect(),
                    named_symbols: criterion_symbols(&super::planning_scope(body)),
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
                named_symbols: BTreeSet::new(),
                routes: BTreeSet::new(),
                terms: terms(
                    &super::planning_scope(scope)
                        .chars()
                        .take(2400)
                        .collect::<String>(),
                ),
            });
        }
        let narrative = super::planning_scope(scope);
        let action_terms = terms(&narrative.to_lowercase());
        let writes = [
            "write",
            "delete",
            "save",
            "insert",
            "update",
            "creation",
            "split",
            "move",
            "persist",
            "copy",
            "deletion",
            "replacement",
        ]
        .iter()
        .any(|t| action_terms.contains(*t));
        let mut invoked_names = HashMap::new();
        if criteria.iter().all(|c| c.label == "Story") {
            let prose = scope.split("```").next().unwrap_or(scope);
            let normalized = |s: &str| -> BTreeSet<String> {
                s.split(|c: char| !c.is_alphanumeric() && c != '_')
                    .filter(|s| s.len() >= 3)
                    .map(|s| call_stem(&s.to_lowercase()).to_owned())
                    .collect()
            };
            let heading = normalized(heading);
            let prose = normalized(prose);
            for (name, args) in super::story_history::call_arguments(scope) {
                let stem = call_stem(&name);
                let score = if heading.contains(stem) {
                    174
                } else if prose.contains(stem) {
                    112
                } else {
                    0
                };
                if score > 0 {
                    invoked_names.insert(name, score + args.len().min(3) * 5);
                }
            }
        }
        let subjects = super::coordinated_story_subjects(scope);
        let main_noun = (subjects.len() == 1)
            .then(|| subjects[0].split_whitespace().last())
            .flatten()
            .filter(|s| !matches!(*s, "action" | "change" | "update"))
            .map(term);
        let subject = subjects.iter().flat_map(|s| terms(s)).collect();
        let excluded = scope
            .lines()
            .take_while(|l| !l.to_lowercase().starts_with("## comments"))
            .flat_map(|line| line.split([';', '.']))
            .filter(|line| {
                let lower = line.to_lowercase();
                [
                    "separately covers",
                    "is not expanded",
                    "keeps its existing",
                    "out of scope",
                    "separately owns",
                    "belongs to the separate",
                ]
                .iter()
                .any(|s| lower.contains(s))
            })
            .flat_map(terms)
            .collect();
        let operations = if writes && subjects.len() >= 2 && criteria.len() >= 2 {
            subjects
                .iter()
                .map(|s| terms(s))
                .filter(|s| s.len() >= 2)
                .collect()
        } else {
            Vec::new()
        };
        Self {
            operation_candidates: vec![BTreeSet::new(); operations.len()],
            operations,
            main_noun,
            subject,
            excluded,
            history: Default::default(),
            source_evidence: HashMap::new(),
            invoked_names,
            invoked: BTreeSet::new(),
            narrative,
            writes,
            transport: BTreeSet::new(),
            server: BTreeSet::new(),
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
        let leaf = name
            .rsplit(['.', ':'])
            .next()
            .unwrap_or(name)
            .to_ascii_lowercase();
        if let Some(&score) = self.invoked_names.get(&leaf)
            && !super::story_history::support_path(path)
            && !engram_core::is_declaration_path(path)
        {
            self.invoked.insert((Reverse(score), id.to_owned()));
            if self.invoked.len() > 32 {
                self.invoked.pop_last();
            }
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

        let lexical = self
            .criteria
            .iter()
            .map(|c| c.terms.intersection(&leaf).count())
            .max()
            .unwrap_or(0);
        let in_leaf = self.title.intersection(&leaf).count();
        if mutator(name.rsplit(['.', ':']).next().unwrap_or(name)) {
            for (action, candidates) in self.operations.iter().zip(&mut self.operation_candidates) {
                let hits = action.intersection(&leaf).count();
                if hits > 0 && !super::story_history::support_path(path) {
                    let priority =
                        hits * 8 + usize::from(is_endpoint(path, owner)) * 5 + title_hits;
                    candidates.insert((Reverse(priority), id.to_owned()));
                    if candidates.len() > 16 {
                        candidates.pop_last();
                    }
                }
            }
        }
        if self.writes && is_script(path) && in_leaf > 0 && lexical >= 2 {
            self.transport
                .insert((Reverse(4 * in_leaf + 5 * lexical), id.to_owned()));
            if self.transport.len() > 16 {
                self.transport.pop_last();
            }
        }
        if self.writes
            && (is_endpoint(path, owner)
                || self
                    .main_noun
                    .as_ref()
                    .is_some_and(|noun| leaf.contains(noun)))
            && lexical > 0
            && mutator(name.rsplit(['.', ':']).next().unwrap_or(name))
        {
            self.server.insert((
                Reverse(4 * title_hits + 3 * in_leaf + 3 * lexical),
                id.into(),
            ));
            if self.server.len() > 16 {
                self.server.pop_last();
            }
        }
        if title_hits == 0 {
            return;
        }
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
        let mut pool: HashMap<String, usize> = HashMap::new();
        for (Reverse(score), id) in self
            .selected
            .iter()
            .chain(&self.server)
            .chain(&self.invoked)
            .chain(self.operation_candidates.iter().flatten())
        {
            let old = pool.entry(id.clone()).or_default();
            *old = (*old).max(*score);
        }
        let prose_issue = self.criteria.iter().all(|c| c.label == "Story");
        for (id, prior) in &self.history.methods {
            // Preserve explicit source and transport anchors. File co-occurrence
            // alone carries less weight than a surviving line from a patch.
            let weight = prior.score
                * (if prose_issue {
                    5
                } else if self.writes {
                    2
                } else {
                    4
                })
                / 5;
            pool.entry(id.clone())
                .and_modify(|s| *s = (*s).max(weight))
                .or_insert(weight);
        }
        let mut pins = HashMap::new();
        static IDENT: LazyLock<regex::Regex> =
            LazyLock::new(|| regex::Regex::new(r"\b[A-Za-z_]\w*(?:\.[A-Za-z_]\w*)*\b").unwrap());
        let mut seen = BTreeSet::new();
        for m in IDENT.find_iter(&self.narrative) {
            let name = m.as_str();
            if !(name.contains('.')
                || name
                    .as_bytes()
                    .windows(2)
                    .any(|p| p[0].is_ascii_lowercase() && p[1].is_ascii_uppercase()))
                || !seen.insert(name.to_lowercase())
            {
                continue;
            }
            if seen.len() > 48 {
                break;
            }
        }
        let resolved = graph
            .unique_named_functions(project, seen.iter().map(String::as_str))
            .unwrap_or_default();
        for (name, n) in resolved {
            pool.insert(n.node_id.clone(), 50);
            pins.insert(n.node_id.clone(), name.to_owned());
            // An unnamed caller in a story (e.g. a page consuming the lookup)
            // is admissible only with a call edge and story/path agreement.
            if let Ok(callers) =
                graph.find_incoming_edges_with_kind(project, Some(EdgeKind::Calls), &n.node_id, 32)
            {
                for (id, _, _) in callers {
                    if let Ok(Some(caller)) = graph.get_node(project, &id) {
                        let file = caller
                            .file_path
                            .as_str()
                            .rsplit('/')
                            .next()
                            .unwrap_or("")
                            .to_lowercase();
                        let stem = file.split('.').next().unwrap_or("");
                        if self
                            .narrative
                            .split_whitespace()
                            .map(|w| {
                                w.trim_matches(|c: char| !c.is_alphanumeric())
                                    .replace('-', "")
                                    .to_lowercase()
                            })
                            .any(|w| w.len() >= 6 && stem.contains(&w))
                        {
                            pool.entry(id)
                                .and_modify(|s| *s = (*s).max(150))
                                .or_insert(150);
                        }
                    }
                }
            }
        }
        // A referenced change may name a method that is absent on the indexed
        // revision. Look in uniquely resolved owner files for its distinctive
        // member words, and cite the actual enclosing declaration.
        for mention in seen.iter().filter(|m| m.contains('.')).take(16) {
            let Some((owner, leaf)) = mention.rsplit_once('.') else {
                continue;
            };
            if pins.values().any(|name| name.eq_ignore_ascii_case(mention)) {
                continue;
            }
            let Some(spelling) = IDENT
                .find_iter(&self.narrative)
                .find(|m| m.as_str().eq_ignore_ascii_case(mention))
            else {
                continue;
            };
            let parts =
                split_symmetric_name_tokens(spelling.as_str().rsplit('.').next().unwrap_or(leaf));
            if parts.len() < 2 {
                continue;
            }
            let needle = parts
                .iter()
                .skip(1)
                .cloned()
                .collect::<String>()
                .to_lowercase();
            if needle.len() < 6 {
                continue;
            }
            let Ok(owners) = graph.query_nodes_by_symbol_name(project, owner, None, 3) else {
                continue;
            };
            let classes: Vec<_> = owners
                .iter()
                .filter(|n| matches!(n.node_type.as_str(), "class" | "struct" | "module"))
                .collect();
            let [owner] = classes.as_slice() else {
                continue;
            };
            if let Ok(methods) =
                graph.query_nodes_in_scope(project, Some("function"), owner.file_path.as_str(), 128)
            {
                for n in methods {
                    if source.lines(&n).is_some_and(|lines| {
                        lines
                            .iter()
                            .any(|(_, s)| s.to_lowercase().contains(&needle))
                    }) {
                        pool.entry(n.node_id)
                            .and_modify(|s| *s = (*s).max(92))
                            .or_insert(92);
                    }
                }
            }
        }
        let mut generated = BTreeSet::new();
        let mut tsconfigs = BTreeSet::new();
        for id in pool.keys() {
            if let Ok(Some(n)) = graph.get_node(project, id) {
                let path = n.file_path.as_str();
                if path.ends_with(".ts") || path.ends_with(".tsx") {
                    for parent in Path::new(path).ancestors().skip(1) {
                        let cfg = parent.join("tsconfig.json");
                        let file = cfg.to_string_lossy().replace('\\', "/");
                        if !tsconfigs.insert(file.clone()) {
                            continue;
                        }
                        if let Ok(text) = crate::planner_source::read(graph, project, root, &file) {
                            static OUTPUT: LazyLock<regex::Regex> = LazyLock::new(|| {
                                regex::Regex::new(r#""outFile"\s*:\s*"([^"]+)""#).unwrap()
                            });
                            if let Some(cap) = OUTPUT.captures(&text) {
                                if let Ok(actual) = root.join(parent).join(&cap[1]).canonicalize() {
                                    generated.insert(actual);
                                }
                            }
                        }
                    }
                }
            }
        }
        // Resolve transport literals in the short-listed client handlers before
        // comparing the handler with the source that actually performs the work.
        let mut routed = Vec::new();
        if self.writes {
            static DEST: LazyLock<regex::Regex> = LazyLock::new(|| {
                regex::Regex::new(r#"(?i)\b(?:ajax|post|put|patch|delete|fetch|request)\s*\(\s*["']([^"']{2,160})["']"#).unwrap()
            });
            let mut scripts = pool.clone();
            for (Reverse(score), id) in &self.transport {
                scripts
                    .entry(id.clone())
                    .and_modify(|s| *s = (*s).max(*score))
                    .or_insert(*score);
            }
            let mut routes = std::collections::BTreeMap::<String, usize>::new();
            for (id, score) in &scripts {
                let Ok(Some(n)) = graph.get_node(project, id) else {
                    continue;
                };
                if !is_script(n.file_path.as_str()) {
                    continue;
                }
                for (_, line) in source.lines(&n).unwrap_or_default() {
                    if let Some(cap) = DEST.captures(&line) {
                        let name = cap[1]
                            .trim_end_matches('/')
                            .rsplit('/')
                            .next()
                            .unwrap_or("");
                        if !name.is_empty() {
                            routes
                                .entry(name.to_ascii_lowercase())
                                .and_modify(|s| *s = (*s).max(*score))
                                .or_insert(*score);
                        }
                    }
                }
            }
            let targets = graph
                .unique_named_functions(project, routes.keys().map(String::as_str))
                .unwrap_or_default();
            for (name, target) in targets {
                if !is_script(target.file_path.as_str()) {
                    routed.push((target.node_id, routes[&name] + 60));
                }
            }
        }
        let routed_files: BTreeSet<_> = routed
            .iter()
            .filter_map(|(id, _)| {
                graph
                    .get_node(project, id)
                    .ok()
                    .flatten()
                    .map(|n| n.file_path.as_str().to_owned())
            })
            .collect();
        for (id, score) in routed {
            let old = pool.entry(id).or_default();
            *old = (*old).max(score);
        }
        if self.writes {
            let files: BTreeSet<_> = pool
                .iter()
                .filter_map(|(id, _)| {
                    let n = graph.get_node(project, id).ok()??;
                    (is_endpoint(n.file_path.as_str(), &n.name) && !is_script(n.file_path.as_str()))
                        .then(|| n.file_path.as_str().to_owned())
                })
                .collect();
            for file in routed_files
                .iter()
                .chain(files.difference(&routed_files))
                .take(6)
            {
                let Ok(nodes) = graph.functions_for_source_lines(project, file) else {
                    continue;
                };
                for n in nodes {
                    let leaf = n.name.rsplit('.').next().unwrap_or(&n.name);
                    if !mutator(leaf) {
                        continue;
                    }
                    let words = terms(leaf);
                    let hits = self
                        .criteria
                        .iter()
                        .map(|c| c.terms.intersection(&words).count())
                        .max()
                        .unwrap_or(0);
                    if hits > 0 {
                        let score = 12 + hits * 5 + usize::from(routed_files.contains(file)) * 26;
                        pool.entry(n.node_id)
                            .and_modify(|s| *s = (*s).max(score))
                            .or_insert(score);
                    }
                }
            }
        }
        let mut call_evidence = HashMap::new();
        if prose_issue && !self.history.methods.is_empty() {
            let mut seeds: Vec<_> = pool
                .iter()
                .filter(|(id, _)| pins.contains_key(*id) || self.history.methods.contains_key(*id))
                .map(|(id, score)| (Reverse(*score), id.clone()))
                .collect();
            seeds.sort();
            seeds.truncate(6);
            let mut best_edges = BTreeSet::new();
            for (_, id) in seeds {
                let Ok(Some(seed)) = graph.get_node(project, &id) else {
                    continue;
                };
                let incoming = graph
                    .find_incoming_edges_with_kind(project, Some(EdgeKind::Calls), &id, 48)
                    .unwrap_or_default();
                let outgoing = graph
                    .neighbors(project, EdgeKind::Calls, &id, 32)
                    .unwrap_or_default();
                for (other, is_caller) in incoming
                    .into_iter()
                    .map(|(id, _, _)| (id, true))
                    .chain(outgoing.into_iter().map(|(id, _)| (id, false)))
                {
                    if other == id || !other.starts_with("sym:function:") {
                        continue;
                    }
                    let Ok(Some(n)) = graph.get_node(project, &other) else {
                        continue;
                    };
                    if crate::services::pre_commit_review_service::is_test_path(
                        n.file_path.as_str(),
                    ) {
                        continue;
                    }
                    let leaf = terms(n.name.rsplit(['.', ':']).next().unwrap_or(&n.name));
                    let title_matches = leaf.intersection(&self.title).count();
                    let prior = self.history.methods.get(&other);
                    if title_matches == 0 && prior.is_none() {
                        continue;
                    }
                    let score = 62
                        + title_matches.min(4) * 24
                        + usize::from(title_matches >= 2) * 28
                        + prior.map_or(0, |w| w.score / 3);
                    let route = if is_caller {
                        format!("{} calls {}", n.name, seed.name)
                    } else {
                        format!("{} calls {}", seed.name, n.name)
                    };
                    best_edges.insert((Reverse(score), other, route));
                }
            }
            for (Reverse(score), id, route) in best_edges.into_iter().take(12) {
                pool.entry(id.clone())
                    .and_modify(|s| *s = (*s).max(score))
                    .or_insert(score);
                call_evidence.entry(id).or_insert(route);
            }
        }
        for (id, witness) in &self.source_evidence {
            let score = pool.entry(id.clone()).or_default();
            *score = (*score).max(witness.score);
        }
        let mut controls = HashMap::new();
        if self.criteria.iter().any(|c| c.terms.contains("control")) {
            let mut editor_files = std::collections::BTreeMap::<String, BTreeSet<String>>::new();
            for id in pool.keys() {
                let Ok(Some(n)) = graph.get_node(project, id) else {
                    continue;
                };
                let words = terms(n.name.rsplit('.').next().unwrap_or(&n.name));
                for action in ["save", "cancel"] {
                    if words.contains(action)
                        && self.criteria.iter().any(|c| c.terms.contains(action))
                    {
                        editor_files
                            .entry(n.file_path.as_str().into())
                            .or_default()
                            .insert(action.into());
                    }
                }
            }
            for (file, _) in editor_files
                .into_iter()
                .filter(|(_, actions)| actions.len() >= 2)
                .take(3)
            {
                for node in graph
                    .functions_for_source_lines(project, &file)
                    .unwrap_or_default()
                {
                    let leaf = terms(node.name.rsplit('.').next().unwrap_or(&node.name));
                    let Some(lines) = source.lines(&node) else {
                        continue;
                    };
                    for criterion in self.criteria.iter().filter(|c| c.terms.contains("control")) {
                        let distinctive: BTreeSet<_> = criterion
                            .terms
                            .iter()
                            .filter(|word| {
                                !self.title.contains(*word)
                                    && ![
                                        "control", "enable", "change", "value", "existing", "open",
                                        "save", "edit",
                                    ]
                                    .contains(&word.as_str())
                            })
                            .cloned()
                            .collect();
                        for (line, text) in &lines {
                            let Some((control, _)) = text.trim().split_once('.') else {
                                continue;
                            };
                            if !control.chars().all(|c| c.is_alphanumeric() || c == '_') {
                                continue;
                            }
                            let words = terms(control);
                            let hits = words.intersection(&distinctive).count();
                            if hits < 2 {
                                continue;
                            }
                            let init = criterion.terms.contains("open")
                                && ["edit", "load", "open"].iter().any(|w| leaf.contains(*w));
                            let rank = 160 + hits * 12 + usize::from(init) * 80;
                            let entry = controls.entry(node.node_id.clone()).or_insert((
                                0,
                                String::new(),
                                0,
                                String::new(),
                            ));
                            if rank > entry.0 {
                                *entry = (rank, criterion.label.clone(), *line, text.clone());
                            }
                        }
                    }
                }
            }
            // One concrete editor control can appear in several handlers. Boost only the
            // best lifecycle match per criterion so that a sibling does not crowd out another action.
            let mut best_controls = std::collections::BTreeMap::new();
            for (id, (score, label, ..)) in &controls {
                let entry = best_controls
                    .entry(label.clone())
                    .or_insert((Reverse(*score), id.clone()));
                *entry = std::cmp::min(entry.clone(), (Reverse(*score), id.clone()));
            }
            let winners: BTreeSet<_> = best_controls.into_values().map(|(_, id)| id).collect();
            controls.retain(|id, _| winners.contains(id));
            for (id, (score, ..)) in &controls {
                pool.entry(id.clone())
                    .and_modify(|s| *s = (*s).max(*score))
                    .or_insert(*score);
            }
        }
        let mut sites = Vec::new();
        let mut owners =
            vec![None::<(usize, String, String, usize, String)>; self.operations.len()];
        for (id, lexical) in &pool {
            let Ok(Some(node)) = graph.get_node(project, id) else {
                continue;
            };
            if node.file_path.as_str().ends_with(".js")
                && root
                    .join(node.file_path.as_str())
                    .canonicalize()
                    .is_ok_and(|p| generated.contains(&p))
            {
                continue;
            }
            let witness = self.source_evidence.get(id);
            let mut lines = match source.lines(&node) {
                Some(lines) => lines,
                None if witness.is_some() => Vec::new(),
                None => continue,
            };
            let invoked = self.invoked_names.contains_key(
                &node
                    .name
                    .rsplit(['.', ':'])
                    .next()
                    .unwrap_or(&node.name)
                    .to_ascii_lowercase(),
            );
            if invoked
                && lines.is_empty()
                && let Some(Some(file)) = source.files.get(node.file_path.as_str())
                && let Some(line) = file.get(node.start_line.saturating_sub(1) as usize)
            {
                lines.push((node.start_line as usize, line.trim().to_owned()));
            }
            let leaf_words = terms(node.name.rsplit(['.', ':']).next().unwrap_or(&node.name));
            let title_matches = self.title.intersection(&leaf_words).count();
            let path = node.file_path.as_str().to_lowercase();
            let type_story = ["typing", "typed", "typescript", "signature", "declaration"]
                .iter()
                .any(|word| self.title.contains(*word))
                || self.title.contains("type");
            if engram_core::is_declaration_path(&path) && !type_story {
                continue;
            }
            let pinned = pins.get(id);
            let mutation = lines.iter().any(|(_, text)| durable_write(text));
            let named: BTreeSet<_> = self
                .criteria
                .iter()
                .filter(|c| c.names(&node.name))
                .map(|c| &c.label)
                .collect();
            if mutation && !self.operations.is_empty() {
                for (index, operation) in self.operations.iter().enumerate() {
                    let Some(criterion) = self
                        .criteria
                        .iter()
                        .filter(|c| operation.is_subset(&c.terms))
                        .min_by_key(|c| {
                            self.operations
                                .iter()
                                .filter(|op| op.is_subset(&c.terms))
                                .count()
                        })
                    else {
                        continue;
                    };
                    let Some((strength, number, excerpt)) =
                        operation_owner(operation, &criterion.terms, &leaf_words, &lines)
                    else {
                        continue;
                    };
                    if (leaf_words.contains("bulk") || leaf_words.contains("batch"))
                        && !criterion.terms.contains("bulk")
                        && !criterion.terms.contains("batch")
                    {
                        continue;
                    }
                    let strength = strength
                        + usize::from(is_endpoint(&path, &node.name))
                            * (4 + usize::from(criterion.terms.contains("manual")) * 18);
                    if owners[index].as_ref().is_none_or(|(best, best_id, ..)| {
                        strength > *best || (strength == *best && id < best_id)
                    }) {
                        owners[index] = Some((
                            strength,
                            id.clone(),
                            criterion.label.clone(),
                            number,
                            excerpt,
                        ));
                    }
                }
            }
            let owner_words = terms(&format!("{} {}", node.name, path));
            let mut subject_hits = self
                .subject
                .intersection(&owner_words)
                .cloned()
                .collect::<BTreeSet<_>>();
            if !self.subject.is_empty() && subject_hits.len() < self.subject.len() {
                for (_, line) in &lines {
                    subject_hits.extend(self.subject.intersection(&terms(line)).cloned());
                    if subject_hits.len() == self.subject.len() {
                        break;
                    }
                }
            }
            let subject_local = !subject_hits.is_empty();
            let main_noun_present = self
                .main_noun
                .as_ref()
                .is_none_or(|word| subject_hits.contains(word));
            let excluded_only: BTreeSet<_> =
                self.excluded.difference(&self.title).cloned().collect();
            let excluded_owner = !excluded_only.is_disjoint(&owner_words);
            for c in &self.criteria {
                if let Some(route) = witness.and_then(|w| w.route.as_ref())
                    && self.criteria.iter().any(|c| c.routes.contains(route))
                    && !c.routes.contains(route)
                {
                    continue;
                }
                if controls
                    .get(id)
                    .is_some_and(|(_, label, ..)| label != &c.label)
                    && named.is_empty()
                {
                    continue;
                }
                if !named.is_empty() && !named.contains(&c.label) {
                    continue;
                }
                let mut scored = Vec::new();
                for (n, text) in &lines {
                    let weight = line_score(text, &c.terms, &self.title)
                        + if self.writes && durable_write(text) {
                            16
                        } else {
                            0
                        };
                    let weight = weight
                        + usize::from(self.history.methods.get(id).is_some_and(|w| w.line == *n))
                            * 8;
                    if weight > 0 {
                        scored.push((Reverse(weight), *n, text));
                    }
                }
                scored.sort();
                // A one-word domain operation may appear only in the declaration.
                // Its indexed start line still gives the agent a navigable implementation.
                if scored.is_empty()
                    && prose_issue
                    && leaf_words.len() == 1
                    && leaf_words
                        .iter()
                        .all(|w| self.title.contains(w) && super::story_token(w).is_some())
                    && let Some(Some(file)) = source.files.get(node.file_path.as_str())
                    && let Some(text) = file.get(node.start_line.saturating_sub(1) as usize)
                {
                    scored.push((Reverse(12), node.start_line as usize, text));
                }
                if scored.is_empty()
                    && invoked
                    && let Some((line, text)) = lines.first()
                {
                    scored.push((Reverse(1), *line, text));
                }
                if scored.is_empty()
                    && witness.is_none()
                    && pinned.is_none()
                    && !self.history.methods.contains_key(id)
                {
                    continue;
                }
                let quality = scored.first().map_or(0, |r| r.0.0);
                let mut rank = *lexical
                    + quality
                    + title_matches * 10
                    + leaf_words.intersection(&c.terms).count() * 5;
                if !prose_issue && named.is_empty() && witness.is_none_or(|w| w.route.is_none()) {
                    if !self.subject.is_empty() && (!subject_local || !main_noun_present) {
                        rank = rank.saturating_sub(125);
                    }
                    if self.excluded.iter().any(|term| {
                        term.len() >= 5
                            && !self.title.contains(term)
                            && owner_words
                                .iter()
                                .any(|w| w.len() > term.len() && w.starts_with(term))
                    }) || excluded_owner
                        && owner_words.intersection(&excluded_only).count()
                            > owner_words.intersection(&c.terms).count()
                    {
                        rank = rank.saturating_sub(120);
                    }
                }
                if !prose_issue
                    && self.subject.len() >= 2
                    && mutation
                    && self
                        .main_noun
                        .as_ref()
                        .is_some_and(|w| leaf_words.contains(w))
                    && leaf_words.intersection(&c.terms).any(|w| mutator(w))
                    && lines.iter().any(|(_, line)| {
                        durable_write(line) && self.subject.is_subset(&terms(line))
                    })
                {
                    // A named command writing the titled subject is stronger than a page reading the same state.
                    rank += 180;
                }
                if !prose_issue
                    && pinned.is_none()
                    && self.writes
                    && !self.title.contains("undo")
                    && !c.terms.contains("undo")
                    && source
                        .files
                        .get(node.file_path.as_str())
                        .and_then(|v| v.as_ref())
                        .and_then(|file| file.get(node.start_line.saturating_sub(1) as usize))
                        .is_some_and(|line| terms(line).contains("undo"))
                {
                    rank = rank.saturating_sub(125);
                }
                if c.terms.contains("page")
                    && (path.contains(".aspx.")
                        || path.contains(".cshtml")
                        || path.contains(".razor"))
                {
                    rank += 15;
                }
                if leaf_words.contains("bulk")
                    && !c.terms.contains("bulk")
                    && !self.title.contains("bulk")
                {
                    rank = rank.saturating_sub(35);
                }
                if (leaf_words.contains("delete") || leaf_words.contains("remove"))
                    && !c.terms.contains("delete")
                    && !c.terms.contains("deletion")
                    && !c.terms.contains("remove")
                {
                    rank = rank.saturating_sub(28);
                }

                if crate::services::pre_commit_review_service::is_test_path(&path)
                    && !self.title.contains("test")
                    && pinned.is_none()
                {
                    rank = rank.saturating_sub(45);
                }
                if self.writes && mutation {
                    rank += 32;
                }
                if self.writes && is_endpoint(node.file_path.as_str(), &node.name) && mutation {
                    rank += 12;
                }
                if let Some(name) = pinned {
                    let subject = terms(name);
                    let in_title = subject.intersection(&self.title).count();
                    rank += if !prose_issue || in_title >= 2 {
                        180
                    } else if in_title == 1 {
                        65
                    } else {
                        28
                    };
                    if terms(name).intersection(&c.terms).count() >= 2 {
                        rank += 18;
                    }
                }
                let mut rows: Vec<_> = scored
                    .into_iter()
                    .take(3)
                    .map(|(_, n, s)| (n, s.to_string()))
                    .collect();
                if let Some(w) = self.history.methods.get(id) {
                    rows.retain(|(line, _)| *line != w.line);
                    rows.insert(0, (w.line, w.text.clone()));
                    rows.truncate(3);
                }
                if let Some(w) = witness {
                    rows.retain(|(line, _)| *line != w.line);
                    rows.insert(0, (w.line, w.text.clone()));
                    rows.truncate(3);
                }
                if let Some((_, label, line, text)) = controls.get(id)
                    && label == &c.label
                {
                    rows.retain(|(n, _)| n != line);
                    rows.insert(0, (*line, text.clone()));
                    rows.truncate(3);
                }
                if rows.is_empty() {
                    rows.push((
                        node.start_line as usize,
                        format!("Indexed declaration {}", node.name),
                    ));
                }
                sites.push((Reverse(rank), node.clone(), c.label.clone(), rows));
            }
        }
        sites.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then(a.1.node_id.cmp(&b.1.node_id))
                .then(a.2.cmp(&b.2))
        });
        let mut chosen = Vec::new();
        let mut used = BTreeSet::new();
        // Retain the best evidenced write owner per requested operation before filling supporting sites.
        for (operation, owner) in self.operations.iter().zip(owners) {
            let Some((_, id, criterion, line, text)) = owner else {
                continue;
            };
            if let Some(i) = sites
                .iter()
                .position(|s| s.1.node_id == id && s.2 == criterion)
            {
                let mut site = sites.remove(i);
                site.3.retain(|(n, _)| *n != line);
                site.3.insert(0, (line, text));
                site.3.truncate(3);
                sites.retain(|s| s.1.node_id != id);
                // For a proven write operation a second client wrapper adds no distinct
                // implementation evidence. Keep UI-specific criteria eligible.
                if !is_script(site.1.file_path.as_str()) {
                    sites.retain(|other| {
                        !is_script(other.1.file_path.as_str())
                            || !operation.is_subset(&terms(
                                other.1.name.rsplit('.').next().unwrap_or(&other.1.name),
                            ))
                            || self.criteria.iter().any(|c| {
                                c.label == other.2
                                    && ["client", "browser", "button", "dialog", "ui"]
                                        .iter()
                                        .any(|t| c.terms.contains(*t))
                            })
                    });
                }
                used.insert(site.2.clone());
                chosen.push(site);
            }
        }
        // Spread comparable matches across criteria. A weak incidental match
        // cannot displace a much stronger source witness solely for diversity.
        while chosen.len() < 3 && !sites.is_empty() {
            let best = sites[0].0.0;
            let idx = sites
                .iter()
                .position(|s| s.0.0 + 12 >= best && !used.contains(&s.2))
                .unwrap_or(0);
            let site = sites.remove(idx);
            sites.retain(|s| {
                s.1.node_id != site.1.node_id
                    && !(prose_issue
                        && s.1.file_path == site.1.file_path
                        && s.1.name == site.1.name)
            });
            used.insert(site.2.clone());
            chosen.push(site);
        }
        chosen
            .into_iter()
            .map(|(_, node, label, lines)| {
                let path = node.file_path.as_str().to_owned();
                let mut evidence: Vec<_> = lines
                    .iter()
                    .map(|(n, s)| format!("{path}:{n}  {s}"))
                    .collect();
                if let Some(w) = self.history.methods.get(&node.node_id) {
                    let basis = if w.patch_match {
                        "added line still present"
                    } else {
                        "story terms in a previously changed file"
                    };
                    evidence.push(format!("History ({basis}): {}", w.change));
                }
                if let Some(w) = self.source_evidence.get(&node.node_id) {
                    evidence.push(format!("Location evidence: {}", w.reason));
                }
                if let Some(route) = call_evidence.get(&node.node_id) {
                    evidence.push(format!("Indexed call path: {route}"));
                }
                evidence.push(format!(
                    "Declaration: {path}:{} (use this start line for edit context)",
                    node.start_line
                ));
                let mut paths = vec![path];
                // Only follow a write actually shown in the selected source. A thin
                // wrapper may delegate once more before touching persistent storage.
                if !self.writes {
                    // The entry point may delegate the behavior to another file.
                    // Surface navigable, indexed callees independently of ranking credit.
                    if let Ok(edges) = graph.neighbors(project, EdgeKind::Calls, &node.node_id, 32)
                    {
                        for (id, _) in edges {
                            let Ok(Some(target)) = graph.get_node(project, &id) else {
                                continue;
                            };
                            let file = target.file_path.as_str();
                            if target.node_type != "function" || paths.iter().any(|p| p == file) {
                                continue;
                            }
                            paths.push(file.to_owned());
                            evidence.push(format!(
                                "{file}:{}  {} (indexed callee of {}; inspect implementation)",
                                target.start_line, target.name, node.name
                            ));
                            if paths.len() == 4 {
                                break;
                            }
                        }
                    }
                }
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
fn criterion_symbols(text: &str) -> BTreeSet<String> {
    static NAME: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"[A-Za-z_]\w*(?:(?:\.|::)[A-Za-z_]\w*)*").unwrap());
    NAME.find_iter(text)
        .filter_map(|m| {
            let word = m.as_str();
            let marked = text[..m.start()].ends_with('\x60') && text[m.end()..].starts_with('\x60');
            let explicit = marked
                || text[m.end()..].trim_start().starts_with('(')
                || word.contains(['.', '_', ':'])
                || word
                    .as_bytes()
                    .windows(2)
                    .any(|p| p[0].is_ascii_lowercase() && p[1].is_ascii_uppercase());
            explicit.then(|| word.to_lowercase().replace("::", "."))
        })
        .collect()
}

impl Criterion {
    fn names(&self, qualified: &str) -> bool {
        let name = qualified.to_lowercase().replace("::", ".");
        let mut suffix = name.as_str();
        loop {
            if self.named_symbols.contains(suffix) {
                return true;
            }
            let Some((_, next)) = suffix.split_once('.') else {
                return false;
            };
            suffix = next;
        }
    }
}

fn operation_owner(
    operation: &BTreeSet<String>,
    criterion: &BTreeSet<String>,
    method: &BTreeSet<String>,
    lines: &[(usize, String)],
) -> Option<(usize, usize, String)> {
    // Two subject terms on one executable line distinguish an owner from a file-wide co-occurrence.
    // A mutation in the enclosing method is required by the caller. Prefer assignments carrying the
    // subject or the old/new values over incidental mentions in a generic write method.
    let mut best = None;
    for (number, line) in lines {
        let words = terms(line);
        let changes_value = ["update", "replace", "convert", "rename", "change"]
            .iter()
            .any(|w| method.contains(*w));
        if words.is_disjoint(operation)
            || !operation.iter().all(|w| {
                words.contains(w) || method.contains(w) || (w == "change" && changes_value)
            })
        {
            continue;
        }
        let replaced_value = words.contains("old") || words.contains("new");
        let assignment =
            line.contains(" = ") && !line.trim_start().to_lowercase().starts_with("if ");
        let changes_existing = ["change", "replacement", "replace"]
            .iter()
            .any(|w| criterion.contains(*w))
            && ["update", "change", "replace", "convert", "rename"]
                .iter()
                .any(|w| method.contains(*w));
        let subject_identifier = line
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|token| token.len() >= 4)
            .any(|token| operation.is_subset(&terms(token)));
        let strength = 28 * usize::from(subject_identifier)
            + 10 * operation.intersection(method).count()
            + 12 * usize::from(changes_existing)
            + 6 * usize::from(replaced_value && assignment)
            + 4 * usize::from(durable_write(line))
            + 2 * usize::from(assignment);
        if best.as_ref().is_none_or(|(score, _, _)| strength > *score) {
            best = Some((strength, *number, line.clone()));
        }
    }
    best
}

pub(super) struct Site {
    pub name: String,
    pub label: String,
    pub lines: Vec<String>,
    pub paths: Vec<String>,
}

fn call_stem(word: &str) -> &str {
    for suffix in ["ing", "ed", "es", "s"] {
        if let Some(stem) = word.strip_suffix(suffix)
            && stem.len() >= 3
        {
            return stem;
        }
    }
    if word.len() > 4 {
        word.strip_suffix('e').unwrap_or(word)
    } else {
        word
    }
}
fn is_script(path: &str) -> bool {
    [".js", ".ts", ".jsx", ".tsx"]
        .iter()
        .any(|ext| path.ends_with(ext))
}
fn is_endpoint(path: &str, owner: &str) -> bool {
    let path = path.to_lowercase();
    let owner = owner.to_lowercase();
    path.split(['/', '.', '-', '_']).any(|s| {
        matches!(
            s,
            "api" | "controller" | "controllers" | "service" | "services"
        )
    }) || owner
        .split(['.', ':'])
        .any(|s| s == "api" || s.ends_with("controller"))
}
fn mutator(name: &str) -> bool {
    super::split_symmetric_name_tokens(name).iter().any(|s| {
        matches!(
            s.as_str(),
            "add"
                | "create"
                | "insert"
                | "update"
                | "save"
                | "write"
                | "replace"
                | "delete"
                | "rename"
                | "move"
                | "split"
                | "copy"
                | "remove"
        )
    })
}
fn durable_write(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        ".submitchanges(",
        ".savechanges(",
        ".insertallonsubmit(",
        ".insertonsubmit(",
        ".deleteonsubmit(",
        ".deleteallonsubmit(",
        ".executecommand(",
        ".executenonquery(",
        ".writetoserver(",
        ".setvalue(",
        ".deletevalue(",
        ".updaterow(",
        ".create(",
        ".commit(",
        "insert into ",
        "delete from ",
    ]
    .iter()
    .any(|s| lower.contains(s))
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
        let end = (node.end_line as usize).min(start + 640);
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
