//! Retrieve earlier landed changes and bind their evidence to current indexed methods.
use super::{planning_scope, split_symmetric_name_tokens};
use crate::services::pre_commit_review_service::is_test_path;
use engram_graph::GraphStore;
use git2::{DiffOptions, Oid, Patch, Repository};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::LazyLock;

fn words(text: &str) -> BTreeSet<String> {
    split_symmetric_name_tokens(text)
        .into_iter()
        .filter_map(|word| {
            if word.len() < 3
                || word.bytes().all(|b| b.is_ascii_digit())
                || matches!(
                    word.as_str(),
                    "the"
                        | "and"
                        | "for"
                        | "with"
                        | "was"
                        | "when"
                        | "are"
                        | "can"
                        | "from"
                        | "not"
                        | "that"
                        | "this"
                        | "does"
                        | "have"
                        | "has"
                        | "would"
                        | "should"
                        | "will"
                        | "but"
                        | "there"
                        | "how"
                        | "what"
                        | "into"
                        | "some"
                        | "using"
                        | "use"
                        | "used"
                        | "new"
                        | "then"
                        | "which"
                        | "also"
                        | "any"
                        | "issue"
                        | "fix"
                        | "fixed"
                        | "bug"
                        | "return"
                        | "function"
                        | "public"
                        | "static"
                        | "var"
                        | "const"
                        | "https"
                        | "http"
                        | "github"
                        | "com"
                        | "before"
                        | "after"
                        | "code"
                        | "version"
                        | "description"
                        | "story"
                        | "criteria"
                        | "user"
                        | "changes"
                        | "must"
                        | "error"
                        | "tests"
                        | "test"
                )
            {
                return None;
            }
            let stem = if word.len() > 4 && word.ends_with('s') && !word.ends_with("ss") {
                &word[..word.len() - 1]
            } else {
                &word
            };
            Some(stem.to_owned())
        })
        .collect()
}

// Keep intact API identifiers as well as their component words. Splitting
// fromFormat into "from"/"format" loses the difference between a parser and a formatter.
fn identifiers(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|s| {
            s.len() >= 4
                && (s
                    .as_bytes()
                    .windows(2)
                    .any(|p| p[0].is_ascii_lowercase() && p[1].is_ascii_uppercase())
                    || s.contains('_'))
        })
        .map(str::to_ascii_lowercase)
        .collect()
}

pub(super) fn call_arguments(story: &str) -> HashMap<String, BTreeSet<String>> {
    static CALL: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"\b([A-Za-z_]\w*)\s*(?:<[^<>\n]{1,100}>)?\s*\(").unwrap()
    });
    static LABEL: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"\b([A-Za-z_]\w*)\s*:(?:=|\s)").unwrap());
    let mut calls = HashMap::<String, BTreeSet<String>>::new();
    for c in CALL.captures_iter(story).take(96) {
        let mut depth = 1;
        let tail = &story[c.get(0).unwrap().end()..];
        let mut end = 0;
        for (i, ch) in tail.char_indices().take(800) {
            match ch {
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {}
            }
            if depth == 0 {
                end = i;
                break;
            }
        }
        let labels = LABEL
            .captures_iter(&tail[..end])
            .map(|c| c[1].to_ascii_lowercase());
        calls
            .entry(c[1].to_ascii_lowercase())
            .or_default()
            .extend(labels);
    }
    calls
}

pub(super) fn support_path(path: &str) -> bool {
    is_test_path(path)
        || path.split('/').any(|p| {
            matches!(
                p.to_ascii_lowercase().as_str(),
                "bench"
                    | "benchmarks"
                    | "benchmark"
                    | "docs"
                    | "examples"
                    | "samples"
                    | "performance"
            )
        })
}

#[derive(Default)]
pub(super) struct Evidence {
    pub methods: HashMap<String, Witness>,
    pub changes: Vec<String>,
    references: Vec<String>,
    pub coverage: String,
}
#[derive(Clone)]
pub(super) struct Witness {
    pub score: usize,
    pub line: usize,
    pub text: String,
    pub change: String,
    pub patch_match: bool,
}
pub(super) struct FetchedPr {
    id: u64,
    oid: Oid,
    title: String,
    description: String,
}
struct Change {
    oid: Oid,
    title: String,
    words: BTreeSet<String>,
    title_words: BTreeSet<String>,
    identifiers: BTreeSet<String>,
    cited: bool,
}
#[derive(Default)]
struct FileEvidence {
    // Index of the best precedent touching this file and each surviving statement.
    first: usize,
    statements: HashMap<String, usize>,
}

pub(super) fn cited_prs(story: &str) -> Vec<u64> {
    static PR: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
            r"(?i)\b(?:PR[\s-]+#?|pull[\s-]+request\s+#?|pullrequest/|pull/)([1-9]\d{0,9})\b",
        )
        .unwrap()
    });
    let mut ids = Vec::new();
    for c in PR.captures_iter(story.split("## Edit history").next().unwrap_or(story)) {
        if let Ok(n) = c[1].parse()
            && !ids.contains(&n)
        {
            ids.push(n);
        }
        if ids.len() == 2 {
            break;
        }
    }
    ids
}

fn change(commit: &git2::Commit, cited: bool) -> Change {
    let title: String = commit.summary().unwrap_or("").chars().take(180).collect();
    let message: String = commit.message().unwrap_or("").chars().take(1200).collect();
    Change {
        oid: commit.id(),
        title_words: words(&title),
        identifiers: identifiers(&message),
        words: words(&message),
        title,
        cited,
    }
}

// Resolve explicit commit and merged-PR references only within the current history.
fn select(
    repo: &Repository,
    head: Oid,
    story: &str,
    fetched: &[FetchedPr],
) -> anyhow::Result<(usize, Vec<Change>)> {
    let prs = cited_prs(story);
    let mut walk = repo.revwalk()?;
    walk.push(head)?;
    walk.simplify_first_parent()?;
    let mut docs = Vec::new();
    static SQUASH: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"\(#(\d+)\)\s*$").unwrap());
    for oid in walk.take(512) {
        let Ok(commit) = oid.and_then(|id| repo.find_commit(id)) else {
            break;
        };
        let summary = commit.summary().unwrap_or("");
        let (id, _) = crate::services::change_card_service::parse_pr_identity(summary, "");
        let pr = id
            .strip_prefix("PR-")
            .and_then(|s| s.parse().ok())
            .or_else(|| SQUASH.captures(summary).and_then(|c| c[1].parse().ok()));
        docs.push(change(&commit, pr.is_some_and(|n| prs.contains(&n))));
    }
    let count = docs.len();
    static SHA: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?i)\b[0-9a-f]{40}\b").unwrap());
    for oid in fetched.iter().map(|pr| pr.oid).chain(
        SHA.find_iter(story)
            .take(3)
            .filter_map(|s| Oid::from_str(s.as_str()).ok()),
    ) {
        if let Some(d) = docs.iter_mut().find(|d| d.oid == oid) {
            d.cited = true;
            continue;
        }
        if repo.graph_descendant_of(head, oid).unwrap_or(false)
            && let Ok(commit) = repo.find_commit(oid)
        {
            docs.push(change(&commit, true));
        }
    }
    for pr in fetched {
        if let Some(d) = docs.iter_mut().find(|d| d.oid == pr.oid) {
            d.title_words.extend(words(&pr.title));
            d.words.extend(words(&pr.description));
            d.identifiers.extend(identifiers(&pr.description));
            d.title = format!("merged PR #{}: {}", pr.id, pr.title);
        }
    }
    let scoped = planning_scope(story);
    let query = words(&scoped.chars().take(6000).collect::<String>());
    let title = words(scoped.lines().next().unwrap_or(""));
    let mut frequency = BTreeMap::new();
    for d in &docs {
        for w in d.words.intersection(&query) {
            *frequency.entry(w.clone()).or_insert(0usize) += 1;
        }
    }
    let headline_ids = identifiers(scoped.lines().next().unwrap_or(""));
    let story_ids = identifiers(&scoped);
    let mut ranked = Vec::new();
    for d in docs {
        let hits: Vec<_> = d.words.intersection(&query).collect();
        let specific = hits
            .iter()
            .any(|w| title.contains(*w) && frequency[*w] <= ((count / 12).max(2)));
        if !d.cited && (hits.len() < 2 && !specific) {
            continue;
        }
        let score = hits
            .iter()
            .map(|w| {
                let rarity = ((count + 1) / (frequency[*w] + 1)).max(1).ilog2() as usize + 1;
                rarity
                    * (1 + usize::from(title.contains(*w)) * 3)
                    * (1 + usize::from(d.title_words.contains(*w)))
            })
            .sum::<usize>();
        let score = score
            + headline_ids.intersection(&d.identifiers).count().min(3) * 75
            + story_ids.intersection(&d.identifiers).count().min(5) * 12;
        ranked.push((std::cmp::Reverse((d.cited, score)), d));
    }
    ranked.sort_by_key(|row| row.0);
    Ok((count, ranked.into_iter().take(5).map(|(_, d)| d).collect()))
}

fn patches(repo: &Repository, changes: &[Change]) -> BTreeMap<String, FileEvidence> {
    let mut files = BTreeMap::new();
    for (rank, change) in changes.iter().enumerate() {
        let Ok(commit) = repo.find_commit(change.oid) else {
            continue;
        };
        let (Ok(parent), Ok(tree)) = (commit.parent(0).and_then(|c| c.tree()), commit.tree())
        else {
            continue;
        };
        let mut opts = DiffOptions::new();
        opts.context_lines(1);
        let Ok(diff) = repo.diff_tree_to_tree(Some(&parent), Some(&tree), Some(&mut opts)) else {
            continue;
        };
        if diff.deltas().len() > 100 {
            continue;
        }
        for (i, delta) in diff.deltas().enumerate() {
            let Some(path) = delta.new_file().path().and_then(Path::to_str) else {
                continue;
            };
            if support_path(path)
                || engram_core::is_declaration_path(path)
                || delta.new_file().id().is_zero()
            {
                continue;
            }

            if !files.contains_key(path) && files.len() >= 24 {
                continue;
            }
            let file = files
                .entry(path.to_string())
                .or_insert_with(|| FileEvidence {
                    first: rank,
                    ..Default::default()
                });
            let Ok(blob) = repo.find_blob(delta.new_file().id()) else {
                continue;
            };
            if blob.size() > 900_000 {
                continue;
            }
            let Ok(Some(patch)) = Patch::from_diff(&diff, i) else {
                continue;
            };
            for h in 0..patch.num_hunks().min(32) {
                let Ok((_, n)) = patch.hunk(h) else { continue };
                for l in 0..n.min(90) {
                    let Ok(line) = patch.line_in_hunk(h, l) else {
                        continue;
                    };
                    if line.origin() != '+' {
                        continue;
                    }
                    let Ok(text) = std::str::from_utf8(line.content()) else {
                        continue;
                    };
                    let text = text.trim();
                    if (14..=350).contains(&text.len())
                        && !text.starts_with(['/', '*', '\'', '#'])
                        && file.statements.len() < 2400
                    {
                        file.statements.entry(text.to_owned()).or_insert(rank);
                    }
                }
            }
        }
    }
    files
}

pub(super) fn collect(
    graph: &GraphStore,
    project: &str,
    root: &Path,
    story: &str,
    fetched: &[FetchedPr],
) -> Evidence {
    let mut ev = Evidence::default();
    let Ok(repo) = Repository::open(root) else {
        ev.coverage = "Git history unavailable in this checkout".into();
        return ev;
    };
    let Some(head) = repo.head().ok().and_then(|h| h.target()) else {
        ev.coverage = "Checkout has no resolved HEAD".into();
        return ev;
    };
    let Ok((count, changes)) = select(&repo, head, story, fetched) else {
        ev.coverage = "Git history could not be read".into();
        return ev;
    };
    ev.coverage = format!(
        "{count} first-parent commits examined at {:.10}; {} earlier changes selected",
        head,
        changes.len()
    );
    for c in &changes {
        ev.changes.push(format!("{:.10} {}", c.oid, c.title));
    }
    // A story may explicitly cite work on a different branch. Report that
    // comparison separately; its line numbers and review state do not describe HEAD.
    static CITED_SHA: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?i)\b[0-9a-f]{40}\b").unwrap());
    let current_story = story.split("## Edit history").next().unwrap_or(story);
    let explicit: BTreeSet<_> = CITED_SHA
        .find_iter(current_story)
        .take(3)
        .filter_map(|m| Oid::from_str(m.as_str()).ok())
        .chain(fetched.iter().map(|p| p.oid))
        .collect();
    for oid in explicit {
        if oid == head || repo.graph_descendant_of(head, oid).unwrap_or(false) {
            continue;
        }
        if let Ok(commit) = repo.find_commit(oid) {
            let mut paths = Vec::new();
            if let (Ok(parent), Ok(tree)) = (commit.parent(0).and_then(|p| p.tree()), commit.tree())
                && let Ok(diff) = repo.diff_tree_to_tree(Some(&parent), Some(&tree), None)
            {
                for d in diff.deltas() {
                    if let Some(path) = d.new_file().path().and_then(Path::to_str)
                        && !support_path(path)
                        && root.join(path).is_file()
                    {
                        paths.push(path.to_owned());
                    }
                    if paths.len() >= 3 {
                        break;
                    }
                }
            }
            let description = commit
                .summary()
                .unwrap_or("")
                .chars()
                .take(150)
                .collect::<String>();
            ev.references.push(format!("Explicit comparison {:.10} ({description}), outside checkout ancestry. First-parent patch paths present in checkout: {}.",oid,paths.join(", ")));
        } else if let Some(pr) = fetched.iter().find(|p| p.oid == oid) {
            ev.references.push(format!(
                "Fetched merged PR #{}: {} ({:.10}); patch unavailable in local Git.",
                pr.id, pr.title, pr.oid
            ));
        }
    }
    let files = patches(&repo, &changes);
    if files.is_empty() {
        return ev;
    }
    let scoped = planning_scope(story);
    let query = words(&scoped.chars().take(6000).collect::<String>());
    let title = words(scoped.lines().next().unwrap_or(""));
    // Discover functions across the selected files in a single graph pass.
    let calls = call_arguments(&scoped);
    let story_ids = identifiers(&scoped);
    let mut starts = BTreeMap::<String, Vec<(usize, String, String)>>::new();
    if graph
        .visit_symbol_names(project, |kind, path, name, id, line| {
            if kind == "function" && files.contains_key(path) {
                starts.entry(path.to_owned()).or_default().push((
                    line as usize,
                    id.to_owned(),
                    name.to_owned(),
                ));
            }
        })
        .is_err()
    {
        return ev;
    }
    let mut nominees = Vec::new();
    for (path, mut funcs) in starts {
        funcs.sort_unstable();
        let file = &files[&path];
        let Ok(source) = crate::planner_source::read(graph, project, root, &path) else {
            continue;
        };
        let mut best = HashMap::<usize, Witness>::new();
        let mut frequency = HashMap::new();
        for line in source.lines() {
            let s = line.trim();
            if file.statements.contains_key(s) {
                *frequency.entry(s).or_insert(0usize) += 1;
            }
        }
        // Sibling start lines alone cannot locate statements that resume in
        // an outer function after an inner callback has ended.
        let mut funcs: Vec<_> = funcs
            .into_iter()
            .filter_map(|(start, id, name)| {
                let node = graph.get_node(project, &id).ok()??;
                Some((start, id, name, node.end_line as usize))
            })
            .collect();
        funcs.sort_by_key(|f| (f.0, std::cmp::Reverse(f.3)));
        let mut next = 0;
        let mut active = Vec::<usize>::new();
        let mut patch_support = HashMap::<usize, usize>::new();
        let mut named_args = HashMap::<usize, usize>::new();
        for (index, line) in source.lines().enumerate() {
            let at = index + 1;
            while active.last().is_some_and(|&i| funcs[i].3 < at) {
                active.pop();
            }
            while next < funcs.len() && funcs[next].0 <= at {
                if funcs[next].3 >= at {
                    active.push(next);
                }
                next += 1;
            }
            let Some(&cursor) = active.last() else {
                continue;
            };
            let text = line.trim();
            if !(6..=360).contains(&text.len()) || text.starts_with(['/', '*', '\'', '#']) {
                continue;
            }
            let prior = file
                .statements
                .get(text)
                .filter(|_| frequency.get(text) == Some(&1));
            if prior.is_some() {
                *patch_support.entry(cursor).or_default() += 1;
            }
            let leaf = funcs[cursor]
                .2
                .rsplit(['.', ':'])
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            if index + 1 <= funcs[cursor].0 + 4
                && let Some(args) = calls.get(&leaf)
            {
                let tokens: BTreeSet<_> = text
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .map(str::to_ascii_lowercase)
                    .collect();
                let matches = tokens.intersection(args).count();
                named_args
                    .entry(cursor)
                    .and_modify(|n| *n = (*n).max(matches))
                    .or_insert(matches);
            }
            let tokens = words(text);
            let overlap = tokens.intersection(&query).count().min(7);
            let subject = tokens.intersection(&title).count().min(4);
            let exact = identifiers(text);
            let concrete = exact.intersection(&story_ids).count().min(4);
            if overlap == 0 || (prior.is_none() && (overlap < 2 || subject == 0) && concrete == 0) {
                continue;
            }
            let rank = prior.copied().unwrap_or(file.first);
            let score = 16
                + (5 - rank) * 3
                + overlap * 4
                + subject * 6
                + concrete * 9
                + usize::from(prior.is_some()) * 28;
            if best.get(&cursor).is_some_and(|w| w.score >= score) {
                continue;
            }
            best.insert(
                cursor,
                Witness {
                    score,
                    line: at,
                    text: text.to_owned(),
                    change: ev.changes[rank].clone(),
                    patch_match: prior.is_some(),
                },
            );
        }
        for (pos, w) in best {
            let (_, id, name, _) = &funcs[pos];
            let leaf = words(name.rsplit(['.', ':']).next().unwrap_or(name));
            let mut witness = w;
            let short = name
                .rsplit(['.', ':'])
                .next()
                .unwrap_or(name)
                .to_ascii_lowercase();
            let landed = patch_support.get(&pos).copied().unwrap_or(0);
            let subject = leaf.intersection(&title).count().min(4);
            witness.score += words(&path).intersection(&title).count().min(3) * 10
                + subject * 14
                + usize::from(subject >= 2) * 22
                + named_args.get(&pos).copied().unwrap_or(0).min(4) * 16
                + landed.min(5) * 7;
            // A prior fix can name a method as the cause or as a caller, even
            // when the retained changed statement sits elsewhere in that file.
            if let Some((rank, _)) = changes
                .iter()
                .enumerate()
                .find(|(_, c)| c.identifiers.contains(&short))
            {
                witness.score += (5 - rank) * 6;
            }
            if !witness.patch_match && landed == 0 && subject < 2 && !calls.contains_key(&short) {
                witness.score = witness.score.saturating_sub(28);
            }
            nominees.push((id.clone(), witness));
        }
    }
    nominees.sort_by(|a, b| b.1.score.cmp(&a.1.score).then(a.0.cmp(&b.0)));
    for (id, w) in nominees.into_iter().take(28) {
        if let Ok(Some(node)) = graph.get_node(project, &id)
            && (node.start_line as usize..=node.end_line as usize).contains(&w.line)
        {
            ev.methods.insert(id, w);
        }
    }
    ev
}

impl Evidence {
    pub(super) fn summary(&self) -> String {
        let mut text = format!("\n## Earlier changes\n{}\n", self.coverage);
        for change in self.changes.iter().take(3) {
            text.push_str(&format!("- {change}\n"));
        }
        for reference in &self.references {
            text.push_str(&format!("- {reference}\\n"));
        }
        text
    }
}

// The endpoint is derived from the registered checkout's origin. Story text
// supplies numeric IDs only. No queries, issue text or source are transmitted.
fn provider_endpoint(remote: &str, id: u64) -> Option<(reqwest::Url, bool)> {
    let normalized = if let Some(path) = remote.strip_prefix("git@github.com:") {
        format!("https://github.com/{path}")
    } else if let Some(path) = remote.strip_prefix("git@ssh.dev.azure.com:v3/") {
        let parts: Vec<_> = path.split('/').collect();
        if parts.len() != 3 {
            return None;
        }
        format!(
            "https://dev.azure.com/{}/{}/_git/{}",
            parts[0], parts[1], parts[2]
        )
    } else {
        remote.to_owned()
    };
    let url =
        reqwest::Url::parse(normalized.trim_end_matches('/').trim_end_matches(".git")).ok()?;
    if url.scheme() != "https" {
        return None;
    }
    let p: Vec<_> = url.path().trim_matches('/').split('/').collect();
    match (url.host_str()?,p.as_slice()){
        ("github.com",[org,repo])=>Some((format!("https://api.github.com/repos/{org}/{repo}/pulls/{id}").parse().ok()?,false)),
        ("dev.azure.com",[org,project,"_git",repo])=>Some((format!("https://dev.azure.com/{org}/{project}/_apis/git/repositories/{repo}/pullrequests/{id}?api-version=7.1").parse().ok()?,true)),
        _=>None,
    }
}
pub(super) async fn resolve_cited_prs(root: &Path, story: &str) -> Vec<FetchedPr> {
    let ids = cited_prs(story);
    if ids.is_empty() {
        return Vec::new();
    }
    let remote = Repository::open(root)
        .ok()
        .and_then(|r| r.find_remote("origin").ok()?.url().map(str::to_owned));
    let Some(remote) = remote else {
        return Vec::new();
    };
    let Ok(client) = reqwest::Client::builder()
        .user_agent("Engram-story-planner")
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(2))
        .build()
    else {
        return Vec::new();
    };
    futures::future::join_all(ids.into_iter().filter_map(|id| {
        let (url, ado) = provider_endpoint(&remote, id)?;
        let mut req = client.get(url);
        if ado && let Some(pat) = super::resolve_ado_pat(None) {
            req = req.basic_auth("", Some(pat));
        }
        Some(async move {
            let mut response = req.send().await.ok()?.error_for_status().ok()?;
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.ok()? {
                if bytes.len() + chunk.len() > 512 * 1024 {
                    return None;
                }
                bytes.extend_from_slice(&chunk);
            }
            let data: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
            let sha = if ado && data["status"] == "completed" {
                data.pointer("/lastMergeCommit/commitId")?.as_str()?
            } else if !ado && data["merged"] == true {
                data["merge_commit_sha"].as_str()?
            } else {
                return None;
            };
            Some(FetchedPr {
                id,
                oid: Oid::from_str(sha).ok()?,
                title: data["title"]
                    .as_str()?
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .chars()
                    .take(180)
                    .collect(),
                description: data[if ado { "description" } else { "body" }]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(2000)
                    .collect(),
            })
        })
    }))
    .await
    .into_iter()
    .flatten()
    .collect()
}
