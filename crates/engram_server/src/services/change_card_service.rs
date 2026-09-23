//! Precedent retrieval: "how did we implement this before?"
//!
//! A developer judges a precedent at the level of a whole published change —
//! the same feature area and the same kind of change — not at the level of
//! one diff hunk or commit message. Measured on 40 frozen stories with 20
//! hand judgements (a judged precedent in the top 3):
//!
//! | ranking                                          | hand top-3 |
//! |--------------------------------------------------|-----------:|
//! | fused words + vectors over messages and diffs    |      11-12 |
//! | one embedding per published change ("card")      |         14 |
//! | cards, top 30 reranked by an LLM judge           |      17-18 |
//!
//! Word matching raised the file-overlap score but lowered the human one:
//! story prose shares words with unrelated code. So precedents are ranked
//! from cards alone and the reranker decides the final order.
//!
//! A card is one first-parent (published) change: its title, description
//! and non-generated changed files. Cards are built from git, embedded with
//! the project's configured embedder (nomic's `search_document:` /
//! `search_query:` prefixes, which that model requires) and cached per
//! repository by commit, so a moving tip only builds its new changes.

use engram_ml::llm_provider::{LlmGenerateOptions, LlmProvider};
use git2::{Oid, Repository};
use regex::Regex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

/// Reranker used when the configured LLM provider is OpenRouter: measured
/// best (18/20) on the precedent benchmark, and the cheapest.
pub const DEFAULT_RERANK_MODEL: &str = "openai/gpt-oss-120b";
/// How many semantic candidates the reranker sees.
pub const RERANK_POOL: usize = 30;
/// Changes touching more non-generated files are bulk (imports, moves,
/// reformats), never a precedent.
pub const BULK_FILES: usize = 150;

const MAX_DESCRIPTION: usize = 1500;
const MAX_CARD_FILES: usize = 40;

/// One published change.
#[derive(Debug, Clone)]
pub struct ChangeCard {
    pub oid: String,
    pub title: String,
    pub description: String,
    pub author: String,
    pub timestamp: u64,
    /// Non-generated changed files (capped at MAX_CARD_FILES).
    pub files: Vec<String>,
    /// Count of non-generated changed files before the cap.
    pub file_count: usize,
    pub vector: Vec<f32>,
}

impl ChangeCard {
    /// The text that is embedded (without the model's document prefix).
    pub fn text(&self) -> String {
        format!(
            "Title: {}\nDescription: {}\nFiles:\n{}",
            self.title,
            self.description,
            self.files.join("\n")
        )
    }
}

/// Build output and dependency files that say nothing about a change's intent.
fn is_generated(path: &str) -> bool {
    static GENERATED: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?i)(\.designer\.(vb|cs)$|\.resx$|\.min\.(js|css)$|\.(js|css)\.map$|(^|/)(bin|obj|node_modules|dist|packages)/|\.(dll|exe|pdb)$|\.refresh$|\.dbml\.layout$|\.(csproj|vbproj|sqlproj|sln)$|packages\.config$|package-lock\.json$|yarn\.lock$|\.lock$)",
        )
        .expect("generated-file pattern")
    });
    GENERATED.is_match(path)
}

/// The first-parent line of `tip`, oldest first.
fn first_parent_line(repo: &Repository, tip: Oid) -> anyhow::Result<Vec<Oid>> {
    let mut line = Vec::new();
    let mut cursor = Some(repo.find_commit(tip)?);
    while let Some(commit) = cursor {
        line.push(commit.id());
        cursor = commit.parent(0).ok();
    }
    line.reverse();
    Ok(line)
}

/// A card's fields, from git (no vector yet).
fn describe(repo: &Repository, oid: Oid) -> anyhow::Result<ChangeCard> {
    let commit = repo.find_commit(oid)?;
    let message = commit.message().unwrap_or("").trim().to_string();
    let summary = commit.summary().unwrap_or("").to_string();
    let hex = oid.to_string();
    let (_, title) = crate::handlers::pr_history_tools::parse_pr_identity(&summary, &hex[..10]);
    let description: String = message
        .lines()
        .skip(1)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .chars()
        .take(MAX_DESCRIPTION)
        .collect();
    let tree = commit.tree()?;
    let parent_tree = commit.parent(0).ok().map(|p| p.tree()).transpose()?;
    let diff = repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), None)?;
    let mut files: Vec<String> = diff
        .deltas()
        .filter_map(|d| d.new_file().path().or_else(|| d.old_file().path()))
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .filter(|p| !is_generated(p))
        .collect();
    files.dedup();
    let file_count = files.len();
    files.truncate(MAX_CARD_FILES);
    Ok(ChangeCard {
        oid: hex,
        title,
        description,
        author: commit.author().name().unwrap_or("").to_string(),
        timestamp: commit.time().seconds().max(0) as u64,
        files,
        file_count,
        vector: Vec::new(),
    })
}

/// Text prepended to documents and queries before embedding. nomic-embed
/// models require `search_document:` / `search_query:`; others take none.
#[derive(Debug, Clone, Copy)]
pub struct EmbedPrefixes {
    pub document: &'static str,
    pub query: &'static str,
}

impl EmbedPrefixes {
    pub fn for_model(model: Option<&str>) -> Self {
        if model.is_some_and(|m| m.to_ascii_lowercase().contains("nomic")) {
            Self {
                document: "search_document: ",
                query: "search_query: ",
            }
        } else {
            Self {
                document: "",
                query: "",
            }
        }
    }
}

/// Cards by commit, per repository; built once, extended as the tip moves.
static CARDS: LazyLock<Mutex<HashMap<PathBuf, HashMap<String, Arc<ChangeCard>>>>> =
    LazyLock::new(Default::default);

/// Cards for every published change on `tip`'s first-parent line, oldest
/// first, embedding only changes not seen before.
pub async fn cards_for_line(
    repo_dir: &Path,
    tip: Oid,
    search: &engram_index::HybridSearchEngine,
    prefixes: EmbedPrefixes,
) -> anyhow::Result<Vec<Arc<ChangeCard>>> {
    let dir = repo_dir.to_path_buf();
    let known: std::collections::HashSet<String> = CARDS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&dir)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    let dir_for_git = dir.clone();
    let (line, fresh) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let repo = engram_git::history::GitWalker::open_repo(&dir_for_git)?;
        let line = first_parent_line(&repo, tip)?;
        let fresh: Vec<ChangeCard> = line
            .iter()
            .filter(|oid| !known.contains(&oid.to_string()))
            .map(|oid| describe(&repo, *oid))
            .collect::<anyhow::Result<_>>()?;
        Ok((line, fresh))
    })
    .await??;
    if !fresh.is_empty() {
        let texts: Vec<String> = fresh
            .iter()
            .map(|c| format!("{}{}", prefixes.document, c.text()))
            .collect();
        let vectors = search.embed_texts(&texts).await?;
        let mut cache = CARDS.lock().unwrap_or_else(|e| e.into_inner());
        let entry = cache.entry(dir.clone()).or_default();
        for (mut card, vector) in fresh.into_iter().zip(vectors) {
            card.vector = vector;
            entry.insert(card.oid.clone(), Arc::new(card));
        }
    }
    let cache = CARDS.lock().unwrap_or_else(|e| e.into_inner());
    let cards = cache.get(&dir).cloned().unwrap_or_default();
    Ok(line
        .iter()
        .filter_map(|oid| cards.get(&oid.to_string()).cloned())
        .collect())
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (norm(a) * norm(b) + 1e-9)
}

/// Cards most similar to the story, bulk changes excluded.
pub async fn semantic_rank(
    story: &str,
    cards: &[Arc<ChangeCard>],
    search: &engram_index::HybridSearchEngine,
    prefixes: EmbedPrefixes,
) -> anyhow::Result<Vec<Arc<ChangeCard>>> {
    let query = search
        .embed_texts(&[format!("{}{story}", prefixes.query)])
        .await?
        .pop()
        .unwrap_or_default();
    let mut scored: Vec<(f32, Arc<ChangeCard>)> = cards
        .iter()
        .filter(|c| c.file_count <= BULK_FILES && !c.vector.is_empty())
        .map(|c| (cosine(&query, &c.vector), Arc::clone(c)))
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    Ok(scored.into_iter().map(|(_, c)| c).collect())
}

fn compact(card: &ChangeCard) -> String {
    let description: String = card
        .description
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(280)
        .collect();
    let files: Vec<&str> = card.files.iter().take(8).map(String::as_str).collect();
    format!(
        "{}\n   {}\n   files: {}",
        card.title,
        description,
        files.join(", ")
    )
}

/// Listwise rerank: the judge names the best precedents among `pool`; they
/// move to the front in its order, the rest keep their semantic order.
pub async fn rerank(
    story: &str,
    pool: Vec<Arc<ChangeCard>>,
    llm: &dyn LlmProvider,
) -> anyhow::Result<Vec<Arc<ChangeCard>>> {
    let listing: Vec<String> = pool
        .iter()
        .enumerate()
        .map(|(i, c)| format!("[{}] {}", i + 1, compact(c)))
        .collect();
    let story: String = story.chars().take(2500).collect();
    let prompt = format!(
        "You are a senior developer on this codebase. A new work item arrives. Which EARLIER \
         changes are its precedents: the same feature area AND the same kind of change, so a \
         developer would copy their approach and files?\n\nWORK ITEM:\n{story}\n\nEARLIER \
         CHANGES:\n{}\n\nAnswer with JSON only: {{\"best\": [numbers of the best precedents, \
         best first, at most 5]}}",
        listing.join("\n")
    );
    let mut options = LlmGenerateOptions::new(4000);
    options.temperature = 0.0;
    let response = llm
        .generate(&prompt, options)
        .await
        .map_err(|e| anyhow::anyhow!("rerank LLM call failed: {e}"))?;
    static OBJECT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\{[^{}]*\}").expect("json object"));
    let picks: Vec<usize> = OBJECT
        .find(&response.text)
        .and_then(|m| serde_json::from_str::<serde_json::Value>(m.as_str()).ok())
        .and_then(|v| v.get("best").cloned())
        .and_then(|v| serde_json::from_value::<Vec<usize>>(v).ok())
        .ok_or_else(|| anyhow::anyhow!("rerank answer was not the requested JSON"))?;
    let mut order: Vec<Arc<ChangeCard>> = Vec::with_capacity(pool.len());
    for pick in picks {
        if let Some(card) = pick.checked_sub(1).and_then(|i| pool.get(i))
            && !order.iter().any(|c| c.oid == card.oid)
        {
            order.push(Arc::clone(card));
        }
    }
    for card in pool {
        if !order.iter().any(|c| c.oid == card.oid) {
            order.push(card);
        }
    }
    Ok(order)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_files_are_not_part_of_a_card() {
        for path in [
            "Site/App_Code/Report.designer.vb",
            "Site/App_GlobalResources/text.resx",
            "scripts/app.min.js",
            "src/bin/Release/app.dll",
            "Web.vbproj",
            "package-lock.json",
        ] {
            assert!(is_generated(path), "{path}");
        }
        for path in [
            "Site/App_Code/Report.vb",
            "scripts/map.ts",
            "db/Tables/roq.sql",
        ] {
            assert!(!is_generated(path), "{path}");
        }
    }

    fn card(oid: &str, files: usize, vector: Vec<f32>) -> Arc<ChangeCard> {
        Arc::new(ChangeCard {
            oid: oid.into(),
            title: oid.into(),
            description: String::new(),
            author: String::new(),
            timestamp: 0,
            files: vec![],
            file_count: files,
            vector,
        })
    }

    #[test]
    fn cosine_prefers_aligned_vectors() {
        assert!(cosine(&[1.0, 0.0], &[1.0, 0.0]) > cosine(&[1.0, 0.0], &[0.0, 1.0]));
        let _ = card("x", 1, vec![1.0]);
    }
}
