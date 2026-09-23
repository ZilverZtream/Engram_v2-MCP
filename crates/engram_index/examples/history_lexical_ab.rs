//! Lexical A/B for history retrieval: character trigrams (`content`) versus
//! words (`content_words`), literal loose and strict, over a copy of a real
//! project index. Each case is a merged change: the query is its text, the
//! cutoff is ancestry of its base commit (no leak), and the score is how many
//! of the files it really changed appear in the top commits retrieved.
//!
//! Opening the index migrates it if it predates `content_words`, so this also
//! times the migration on real data. Run against a COPY:
//!
//!   cargo run --release -p engram_index --example history_lexical_ab -- \
//!     <index_copy_dir> <cases.json> <repo_dir> <project_id> [title|full]
//!
//! cases.json: [{"pr","base","title","desc","files":[...]}]

use engram_index::literal_text_query;
use engram_index::tantivy_index::open_or_create;
use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::time::Instant;
use tantivy::TantivyDocument;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, TermQuery};
use tantivy::schema::{IndexRecordOption, Term, Value};

const TOP_COMMITS: usize = 10;

fn git(repo: &str, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()?;
    anyhow::ensure!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn canon(path: &str) -> String {
    path.replace('\\', "/")
        .trim_start_matches('/')
        .to_lowercase()
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    anyhow::ensure!(
        args.len() >= 5,
        "usage: <index_dir> <cases.json> <repo> <project_id> [title|full]"
    );
    let (index_dir, cases_path, repo, project) = (&args[1], &args[2], &args[3], &args[4]);
    let full_text = args.get(5).is_some_and(|m| m == "full");

    let started = Instant::now();
    let (index, fields) = open_or_create(std::path::Path::new(index_dir))?;
    let searcher = index.reader()?.searcher();
    eprintln!(
        "open (+migration if needed): {:.1?}, {} live docs",
        started.elapsed(),
        searcher.num_docs()
    );

    let cases: Vec<serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(cases_path)?)?;
    let arms = [
        ("trigram loose", fields.content, false),
        ("words   loose", fields.content_words, false),
        ("trigram strict", fields.content, true),
        ("words   strict", fields.content_words, true),
    ];
    let mut commit_files: HashMap<String, Vec<String>> = HashMap::new();
    let mut totals = vec![(0.0f64, 0usize, 0usize, 0u128); arms.len()];
    let mut scored = 0usize;
    for case in &cases {
        let base = case["base"].as_str().unwrap_or_default();
        let Ok(reachable) = git(repo, &["rev-list", base]) else {
            continue;
        };
        let reachable: HashSet<&str> = reachable.lines().collect();
        let truth: HashSet<String> = case["files"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|f| f.as_str().map(canon))
            .collect();
        let mut text = case["title"].as_str().unwrap_or_default().to_string();
        if full_text {
            text.push(' ');
            text.push_str(case["desc"].as_str().unwrap_or_default());
        }
        scored += 1;
        for (arm, (_, field, conjunction)) in arms.iter().enumerate() {
            let t = Instant::now();
            let query = BooleanQuery::new(vec![
                (
                    Occur::Must,
                    literal_text_query(&index, *field, &text, *conjunction)?,
                ),
                (
                    Occur::Must,
                    Box::new(TermQuery::new(
                        Term::from_field_text(fields.project_id, project),
                        IndexRecordOption::Basic,
                    )) as Box<dyn Query>,
                ),
                (
                    Occur::Must,
                    Box::new(TermQuery::new(
                        Term::from_field_text(fields.namespace, "history"),
                        IndexRecordOption::Basic,
                    )),
                ),
            ]);
            let top = searcher.search(&query, &TopDocs::with_limit(500))?;
            let mut commits: Vec<String> = Vec::new();
            for (_, addr) in top {
                let doc: TantivyDocument = searcher.doc(addr)?;
                let path = doc
                    .get_first(fields.path)
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let oid = path
                    .strip_prefix("commit:")
                    .or_else(|| path.strip_prefix("diff:").and_then(|r| r.split(':').next()));
                if let Some(oid) = oid
                    && reachable.contains(oid)
                    && !commits.iter().any(|c| c == oid)
                {
                    commits.push(oid.to_string());
                    if commits.len() == TOP_COMMITS {
                        break;
                    }
                }
            }
            let elapsed = t.elapsed().as_millis();
            let mut found: HashSet<String> = HashSet::new();
            for oid in &commits {
                if !commit_files.contains_key(oid) {
                    let files = git(repo, &["show", "--name-only", "--format=", oid])?
                        .lines()
                        .map(canon)
                        .collect();
                    commit_files.insert(oid.clone(), files);
                }
                found.extend(
                    commit_files[oid]
                        .iter()
                        .filter(|f| truth.contains(*f))
                        .cloned(),
                );
            }
            let recall = found.len() as f64 / truth.len().max(1) as f64;
            let slot = &mut totals[arm];
            slot.0 += recall;
            slot.1 += usize::from(!found.is_empty());
            slot.2 += commits.len();
            slot.3 += elapsed;
        }
    }
    println!(
        "{scored} cases, query = {}",
        if full_text {
            "title + description"
        } else {
            "title"
        }
    );
    println!(
        "{:<15} {:>10} {:>12} {:>13} {:>8}",
        "arm", "recall@10", "any-hit@10", "commits/case", "ms/q"
    );
    for ((name, _, _), (recall, hits, commits, ms)) in arms.iter().zip(&totals) {
        let n = scored.max(1) as f64;
        println!(
            "{name:<15} {:>10.3} {:>12} {:>13.1} {:>8.1}",
            recall / n,
            format!("{hits}/{scored}"),
            *commits as f64 / n,
            *ms as f64 / n
        );
    }
    Ok(())
}
