//! Source evidence for story localization, independent of method-name similarity.
use super::{planning_scope, story_history::support_path, story_sites::terms, story_token};
use engram_graph::{GraphStore, Node};
use engram_index::{
    HybridQuery, HybridSearchEngine,
    grep::{self, FreshnessMode, GrepQuery},
};
use regex::Regex;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::Path,
    sync::LazyLock,
};

pub(super) struct Witness {
    pub line: usize,
    pub text: String,
    pub reason: String,
    pub route: Option<String>,
    pub score: usize,
}

#[derive(Default)]
struct FileHits {
    quotes: BTreeMap<usize, String>,
    chunks: Vec<(usize, usize)>,
    frames: Vec<(usize, String)>,
}

fn production(path: &str) -> bool {
    !support_path(path)
        && !path
            .split('/')
            .any(|s| matches!(s, "dist" | "node_modules" | "vendor"))
        && !path.ends_with(".min.js")
}

fn quotations(scope: &str) -> Vec<&str> {
    let mut fenced = false;
    let mut quotes = Vec::new();
    for line in scope.lines().map(str::trim) {
        if line.starts_with("~~~") || line.starts_with("\x60\x60\x60") {
            fenced = !fenced;
        } else if fenced
            && (24..=220).contains(&line.len())
            && line.contains(['=', '(', '.'])
            && !["at ", "//", "http"].iter().any(|p| line.starts_with(p))
            && !quotes.contains(&line)
        {
            quotes.push(line);
        }
    }
    quotes.truncate(20);
    quotes
}

fn prefer(out: &mut HashMap<String, Witness>, node: &Node, w: Witness) {
    if out.get(&node.node_id).is_none_or(|old| old.score < w.score) {
        out.insert(node.node_id.clone(), w);
    }
}

fn documented_method<'a>(methods: &'a [Node], lines: &[&str], line: usize) -> Option<&'a Node> {
    fn comment(s: &str) -> bool {
        s.starts_with('*') || s.starts_with("///") || s.starts_with("'''")
    }
    if !comment(lines.get(line.checked_sub(1)?)?.trim_start()) {
        return None;
    }
    let next = methods
        .iter()
        .filter(|n| n.start_line as usize > line)
        .min_by_key(|n| n.start_line)?;
    let gap = lines.get(line..next.start_line as usize - 1)?;
    gap.iter()
        .all(|s| s.trim().is_empty() || comment(s.trim_start()))
        .then_some(next)
}

fn owner(methods: &[Node], line: usize) -> Option<&Node> {
    methods
        .iter()
        .filter(|n| (n.start_line as usize..=n.end_line as usize).contains(&line))
        .min_by_key(|n| n.end_line.saturating_sub(n.start_line))
}

fn operation_words(word: &str) -> &'static [&'static str] {
    match word {
        "hide" | "hidden" | "disappear" | "visible" | "visibility" => {
            &["visible", "hidden", "display", "visibility"]
        }
        "reject" | "refuse" | "deny" | "invalid" | "validate" => {
            &["throw", "error", "validate", "forbidden", "invalid"]
        }
        "save" | "persist" | "write" => &["save", "insert", "update", "commit", "submit"],
        "remove" | "delete" => &["remove", "delete", "drop"],
        "fallback" | "default" => &["fallback", "default"],
        _ => &[],
    }
}

pub(super) fn collect(
    graph: &GraphStore,
    engine: &HybridSearchEngine,
    root: &Path,
    project: &str,
    generation: u64,
    story: &str,
) -> HashMap<String, Witness> {
    let scope = planning_scope(story);
    let mut files = BTreeMap::<String, FileHits>::new();
    for quote in quotations(&scope) {
        let query = GrepQuery {
            project_id: project.into(),
            namespace: "memory".into(),
            generation,
            pattern: quote.into(),
            regex: false,
            case_sensitive: Some(true),
            multiline: false,
            path_prefix: None,
            exclude_path_prefixes: Vec::new(),
            language: None,
            context_before: 0,
            context_after: 0,
            max_results: 12,
            freshness: FreshnessMode::Off,
        };
        let Ok(result) = grep::grep(engine, root, &query, || Ok(Vec::new())) else {
            continue;
        };
        if result.matches.len() >= query.max_results {
            continue;
        }
        let locations: BTreeSet<_> = result
            .matches
            .into_iter()
            .filter(|m| production(&m.file_path))
            .map(|m| (m.file_path, m.line as usize))
            .collect();
        if locations.len() == 1 {
            let (path, line) = locations.into_iter().next().unwrap();
            files
                .entry(path)
                .or_default()
                .quotes
                .insert(line, quote.into());
        }
    }

    static FRAME: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
        r"(?m)^\s*at\s+(?:async\s+)?([\w.$:]+)\s*(?:\(([^\n()]+?):\d+:\d+\)|\([^\n]*?\)\s+in\s+([^\n]+?):line\s+\d+)"
    ).unwrap()
    });
    let frames: Vec<_> = FRAME
        .captures_iter(&scope)
        .take(20)
        .map(|c| {
            let leaf = c[1].rsplit(['.', ':']).next().unwrap_or("").to_lowercase();
            let path = c
                .get(2)
                .or_else(|| c.get(3))
                .unwrap()
                .as_str()
                .replace('\\', "/")
                .to_lowercase();
            (leaf, path)
        })
        .collect();
    let title = scope.lines().find(|s| !s.trim().is_empty()).unwrap_or("");
    let subjects: BTreeSet<_> = terms(title)
        .into_iter()
        .filter(|s| story_token(s).is_some())
        .collect();
    let mut queries = Vec::new();
    if !subjects.is_empty() {
        queries.push(subjects.clone());
    }
    let description = scope
        .lines()
        .skip_while(|line| *line != title)
        .skip(1)
        .take_while(|line| !line.trim().starts_with("\x60\x60\x60"))
        .find(|line| {
            (30..=500).contains(&line.len())
                && !line.trim().starts_with(['<', '#', '!'])
                && !line.contains("://")
                && !line.contains('|')
        });
    if let Some(line) = description {
        let words: BTreeSet<_> = super::split_symmetric_name_tokens(line)
            .into_iter()
            .filter_map(|s| story_token(&s))
            .take(14)
            .flat_map(|s| terms(&s))
            .collect();
        if words.len() >= 2 && words != subjects {
            queries.push(words);
        }
    }
    let all_terms: BTreeSet<_> = queries.iter().flatten().cloned().collect();
    let operations: BTreeSet<_> = terms(title)
        .into_iter()
        .chain(description.into_iter().flat_map(terms))
        .collect::<BTreeSet<_>>()
        .iter()
        .flat_map(|s| operation_words(s).iter().copied())
        .collect();
    for query_terms in &queries {
        let query = HybridQuery {
            project_id: project.into(),
            namespace: "memory".into(),
            generation,
            text: query_terms.iter().cloned().collect::<Vec<_>>().join(" "),
            top_k: 32,
            fts_mode: "loose".into(),
            include_path_prefixes: None,
            exclude_path_prefixes: None,
            include_path_suffixes: None,
            language_filters: None,
            author_filter: None,
            date_after: None,
            date_before: None,
            use_mmr: false,
        };
        if let Ok(hits) = engine.lexical_search_paths(&query, &production) {
            for hit in hits {
                let path = hit.path.as_str();
                if production(path)
                    && hit.start_line > 0
                    && (files.contains_key(path) || files.len() < 16)
                {
                    files
                        .entry(path.into())
                        .or_default()
                        .chunks
                        .push((hit.start_line as usize, hit.end_line as usize));
                }
            }
        }
    }
    let mut stack = BTreeMap::<usize, Vec<(Node, Witness)>>::new();
    if !frames.is_empty() {
        let _ = graph.visit_symbol_names(project, |kind, path, name, id, _| {
            if kind != "function" {
                return;
            }
            let leaf = name.rsplit(['.', ':']).next().unwrap_or(name);
            for (i, (method, frame_path)) in frames.iter().enumerate() {
                if leaf.eq_ignore_ascii_case(method) && production(path) {
                    let local = path.to_lowercase();
                    if frame_path == &local
                        || frame_path
                            .strip_suffix(&local)
                            .is_some_and(|s| s.ends_with('/'))
                    {
                        files
                            .entry(path.into())
                            .or_default()
                            .frames
                            .push((i, id.into()));
                    }
                }
            }
        });
    }
    let mut out = HashMap::new();
    for (path, hits) in files {
        let Ok(text) = crate::planner_source::read(graph, project, root, &path) else {
            continue;
        };
        let Ok(methods) = graph.functions_for_source_lines(project, &path) else {
            continue;
        };
        let lines: Vec<_> = text.lines().collect();
        for (i, id) in hits.frames {
            if let Some(n) = methods.iter().find(|n| n.node_id == id)
                && let Some(line) = n
                    .start_line
                    .checked_sub(1)
                    .and_then(|n| lines.get(n as usize))
            {
                stack.entry(i).or_default().push((
                    n.clone(),
                    Witness {
                        line: n.start_line as usize,
                        text: line.trim().into(),
                        route: None,
                        reason:
                            "Stack frame matched by project path and method name in indexed source."
                                .into(),
                        score: 420,
                    },
                ));
            }
        }
        for (line, quote) in hits.quotes {
            let Some(text) = line
                .checked_sub(1)
                .and_then(|i| lines.get(i))
                .map(|s| s.trim())
            else {
                continue;
            };
            if text.len() > 500 || !text.contains(&quote) {
                continue;
            }
            let doc = documented_method(&methods, &lines, line);
            if let Some(n) = doc.or_else(|| owner(&methods, line)) {
                prefer(
                    &mut out,
                    n,
                    Witness {
                        line,
                        text: text.into(),
                        score: 430,
                        route: None,
                        reason: if doc.is_some() {
                            "The story quotes this declaration's documentation."
                        } else {
                            "The story quotes this statement in the indexed implementation."
                        }
                        .into(),
                    },
                );
            }
        }
        let path_terms = terms(path.rsplit('/').next().unwrap_or(&path));
        for (start, end) in hits.chunks {
            for line in start..=end.min(lines.len()).min(start + 180) {
                let text = lines[line - 1].trim();
                if text.len() > 360 || text.len() < 6 || text.starts_with(['/', '*', '#', '\'']) {
                    continue;
                }
                let words = terms(text);
                let heading_hits = subjects.intersection(&words).count();
                let common = all_terms.intersection(&words).count();
                let operation = operations.iter().any(|s| words.contains(*s));
                if common == 0 && !operation {
                    continue;
                }
                let Some(n) = owner(&methods, line) else {
                    continue;
                };
                let method_terms = terms(&n.name);
                let context = subjects
                    .iter()
                    .filter(|s| method_terms.contains(*s) || path_terms.contains(*s))
                    .count();
                if (heading_hits == 0 && context == 0)
                    || (common < 2 && !(operation && context > 0))
                {
                    continue;
                }
                let score = 32
                    + heading_hits.min(4) * 13
                    + common.min(5) * 9
                    + context.min(3) * 14
                    + usize::from(operation) * 34;
                prefer(
                    &mut out,
                    n,
                    Witness {
                        line,
                        text: text.into(),
                        score,
                        reason:
                            "Story terms matched this source line in the enclosing declaration."
                                .into(),
                        route: None,
                    },
                );
            }
        }
    }
    if let Some((n, w)) = stack
        .into_values()
        .find_map(|mut v| (v.len() == 1).then(|| v.pop().unwrap()))
    {
        prefer(&mut out, &n, w);
    }
    super::story_routes::extend(&mut out, graph, engine, project, root, generation, &scope);
    out
}
