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
//! `search_query:` prefixes, which that model requires). Bounded retention is
//! keyed by repository, commit and document embedding profile; a moving tip
//! builds only missing changes. Disabled vectors use a lexical baseline.

use engram_ml::llm_provider::{LlmGenerateOptions, LlmProvider};
use git2::{Oid, Repository};
use regex::Regex;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};
mod cache;
mod metadata_cache;
#[cfg(all(test, feature = "vector"))]
#[path = "change_card_service/restart_measurement.rs"]
mod restart_measurement;

/// Reranker used when the configured LLM provider is OpenRouter: measured
/// best (18/20) on the precedent benchmark, and the cheapest.
pub const DEFAULT_RERANK_MODEL: &str = "openai/gpt-oss-120b";
/// How many semantic candidates the reranker sees.
pub const RERANK_POOL: usize = 30;
pub const DEFAULT_RERANK_TIMEOUT_MS: u64 = 90_000;
pub const MAX_RERANK_TIMEOUT_MS: u64 = 90_000;

pub fn checked_rerank_timeout_ms(value: Option<u64>) -> anyhow::Result<u64> {
    let ms = value.unwrap_or(DEFAULT_RERANK_TIMEOUT_MS);
    anyhow::ensure!(
        ms <= MAX_RERANK_TIMEOUT_MS,
        "rerank_timeout_ms must be between 0 and 90000"
    );
    Ok(ms)
}

#[derive(Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RerankOutcome {
    Disabled,
    ZeroBudget,
    NoCandidates,
    Unavailable,
    Completed { model: String, pool_size: usize },
    Failed { reason: String },
    TimedOut { budget_ms: u64 },
}

pub struct RerankResult {
    pub ranked: Vec<Arc<ChangeCard>>,
    pub outcome: RerankOutcome,
}

/// Only optional LLM ordering is bounded here; baseline retrieval is separate.
/// Every failure preserves the full baseline, including candidates outside the pool.
pub async fn rerank_with_budget<F>(
    story: &str,
    baseline: Vec<Arc<ChangeCard>>,
    enabled: bool,
    timeout_ms: u64,
    provider: F,
) -> anyhow::Result<RerankResult>
where
    F: FnOnce() -> Option<(Arc<dyn LlmProvider>, String)>,
{
    checked_rerank_timeout_ms(Some(timeout_ms))?;
    let skipped = if !enabled {
        Some(RerankOutcome::Disabled)
    } else if timeout_ms == 0 {
        Some(RerankOutcome::ZeroBudget)
    } else if baseline.is_empty() {
        Some(RerankOutcome::NoCandidates)
    } else {
        None
    };
    if let Some(outcome) = skipped {
        return Ok(RerankResult {
            ranked: baseline,
            outcome,
        });
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    let Some((llm, model)) = provider() else {
        return Ok(RerankResult {
            ranked: baseline,
            outcome: RerankOutcome::Unavailable,
        });
    };
    let pool_size = baseline.len().min(RERANK_POOL);
    let pool = baseline[..pool_size].to_vec();
    let outcome = match tokio::time::timeout_at(deadline, rerank(story, pool, llm.as_ref())).await {
        Ok(Ok(mut reordered)) => {
            reordered.extend(baseline.into_iter().skip(pool_size));
            return Ok(RerankResult {
                ranked: reordered,
                outcome: RerankOutcome::Completed { model, pool_size },
            });
        }
        Ok(Err(error)) => RerankOutcome::Failed {
            reason: error.to_string().chars().take(240).collect(),
        },
        Err(_) => RerankOutcome::TimedOut {
            budget_ms: timeout_ms,
        },
    };
    Ok(RerankResult {
        ranked: baseline,
        outcome,
    })
}
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
        cursor = if commit.parent_count() == 0 {None} else {Some(commit.parent(0)?)};
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
    let parent_tree = if commit.parent_count() == 0 { None } else {
        Some(commit.parent(0)?.tree()?)
    };
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

/// A retention bound, not a whole-request/process memory budget. Large request
/// results remain usable without retaining an oversized partition globally.
static CARDS: LazyLock<Mutex<cache::CardCache>> =
    LazyLock::new(|| Mutex::new(cache::CardCache::new(8, 32 * 1024 * 1024)));

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct CardBuildStats {
    git_open_ns: u64,
    first_parent_walk_ns: u64,
    describe_ns: u64,
    described_commits: usize,
    line_commits: usize,
    fresh_embedding_texts: usize,
    estimated_retained_bytes: usize,
    metadata_cache_lookups: usize,
    metadata_cache_hits: usize,
    metadata_cache_writes: usize,
    metadata_cache_evictions: usize,
    metadata_cache_skipped: usize,
    metadata_cache_errors: usize,
}

/// Cards for every published change on `tip`'s first-parent line, oldest
/// first, embedding only changes not seen before.
pub async fn cards_for_line(
    repo_dir: &Path,
    tip: Oid,
    search: &engram_index::HybridSearchEngine,
    prefixes: EmbedPrefixes,
) -> anyhow::Result<Vec<Arc<ChangeCard>>> {
    Ok(cards_for_line_measured(repo_dir, tip, search, prefixes).await?.0)
}

/// Application path: reuse the existing Registry handle for bounded derived
/// metadata. Embedding profiles and vectors stay in their existing caches.
pub async fn cards_for_line_cached(
    repo_dir: &Path, tip: Oid, search: &engram_index::HybridSearchEngine,
    prefixes: EmbedPrefixes, registry: &engram_core::Registry,
) -> anyhow::Result<Vec<Arc<ChangeCard>>> {
    Ok(cards_for_line_observed(repo_dir,tip,search,prefixes,Some(registry.clone())).await?.0)
}

async fn cards_for_line_measured(
    repo_dir: &Path,
    tip: Oid,
    search: &engram_index::HybridSearchEngine,
    prefixes: EmbedPrefixes,
) -> anyhow::Result<(Vec<Arc<ChangeCard>>, CardBuildStats)> {
    cards_for_line_observed(repo_dir,tip,search,prefixes,None).await
}

async fn cards_for_line_observed(
    repo_dir: &Path, tip: Oid, search: &engram_index::HybridSearchEngine,
    prefixes: EmbedPrefixes, registry: Option<engram_core::Registry>,
) -> anyhow::Result<(Vec<Arc<ChangeCard>>, CardBuildStats)> {
    // Capability, not cached data, decides whether vectors may participate.
    // A no-vector build or fts_only project never constructs placeholder cards.
    if search.semantic_quality() == engram_index::hybrid::SemanticQuality::Off {
        let root = repo_dir.to_path_buf();
        return tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let (_, cards, stats) = metadata_cache::read(&root, tip, &Default::default(),registry.as_ref().map(|r|r as &dyn metadata_cache::Store))?;
            Ok((cards.into_iter().map(Arc::new).collect(), stats))
        })
        .await?;
    }
    let root = repo_dir.to_path_buf();
    let dir = tokio::task::spawn_blocking(move || std::fs::canonicalize(root)).await??;
    let (identity, dimension) = search
        .document_embedding_identity()
        .ok_or_else(|| anyhow::anyhow!("document embedding capability is unavailable"))?;
    let mut profile = blake3::Hasher::new();
    for part in [identity.as_str(), prefixes.document] {
        profile.update(&(part.len() as u64).to_le_bytes());
        profile.update(part.as_bytes());
    }
    let key = cache::Key {
        repository: dir.clone(),
        profile: profile.finalize().to_hex().to_string(),
    };
    // This request owns its cards through Git/provider awaits and eviction.
    let mut cards = CARDS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .snapshot(&key);
    let known: std::collections::HashSet<String> = cards.keys().cloned().collect();
    let dir_for_git = dir.clone();
    let (line, fresh, mut stats) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        metadata_cache::read(&dir_for_git, tip, &known,registry.as_ref().map(|r|r as &dyn metadata_cache::Store))
    })
    .await??;
    if !fresh.is_empty() {
        let texts: Vec<String> = fresh
            .iter()
            .map(|c| format!("{}{}", prefixes.document, c.text()))
            .collect();
        let vectors = search.embed_texts(&texts).await?;
        stats.fresh_embedding_texts = texts.len();
        anyhow::ensure!(
            vectors.len() == fresh.len(),
            "change card provider returned an incomplete batch"
        );
        anyhow::ensure!(
            vectors
                .iter()
                .all(|vector| vector.len() == dimension && vector.iter().all(|v| v.is_finite())),
            "change card provider returned incompatible vectors"
        );
        for (mut card, vector) in fresh.into_iter().zip(vectors) {
            card.vector = vector;
            cards.insert(card.oid.clone(), Arc::new(card));
        }
    }
    let result =
        line.iter()
            .map(|oid| {
                cards.get(&oid.to_string()).cloned().ok_or_else(|| {
                    anyhow::anyhow!("change card missing for requested commit {oid}")
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
    let mut retained = CARDS.lock().unwrap_or_else(|e| e.into_inner());
    retained.retain(key.clone(), cards);
    stats.estimated_retained_bytes = retained.retained_bytes(&key);
    Ok((result, stats))
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (norm(a) * norm(b) + 1e-9)
}

/// Explicit lexical fallback over the already scoped first-parent history.
/// Unmatched changes are excluded rather than presented as relevant precedents.
pub fn lexical_rank(story: &str, cards: &[Arc<ChangeCard>]) -> Vec<Arc<ChangeCard>> {
    let terms: std::collections::BTreeSet<_> = story
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .take(256)
        .map(str::to_lowercase)
        .collect();
    let mut scored = Vec::new();
    for card in cards.iter().filter(|c| c.file_count <= BULK_FILES) {
        let words = |text: &str| -> std::collections::BTreeSet<String> {
            text.split(|c: char| !c.is_alphanumeric())
                .filter(|s| !s.is_empty())
                .map(str::to_lowercase)
                .collect()
        };
        let title = words(&card.title);
        let description = words(&card.description);
        let files = words(&card.files.join(" "));
        let score = terms
            .iter()
            .map(|term| {
                3 * usize::from(title.contains(term))
                    + usize::from(description.contains(term))
                    + 2 * usize::from(files.contains(term))
            })
            .sum::<usize>();
        if score > 0 {
            scored.push((score, card.clone()));
        }
    }
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.timestamp.cmp(&a.1.timestamp))
            .then_with(|| a.1.oid.cmp(&b.1.oid))
    });
    scored.into_iter().map(|(_, card)| card).collect()
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
    anyhow::ensure!(picks.len() <= 5, "rerank returned more than five picks");
    let mut seen = vec![false; pool.len()];
    let mut order: Vec<Arc<ChangeCard>> = Vec::with_capacity(pool.len());
    for pick in picks {
        let index = pick
            .checked_sub(1)
            .filter(|&i| i < pool.len())
            .ok_or_else(|| anyhow::anyhow!("rerank returned an out-of-range pick"))?;
        anyhow::ensure!(!seen[index], "rerank returned a duplicate pick");
        seen[index] = true;
        order.push(Arc::clone(&pool[index]));
    }
    for (index, card) in pool.into_iter().enumerate() {
        if !seen[index] {
            order.push(card);
        }
    }
    Ok(order)
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_ml::llm_provider::{LlmError, LlmResponse};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[cfg(feature = "vector")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cached_history_vectors_follow_each_configuration_during_switching_and_concurrent_requests()
    -> anyhow::Result<()> {
        use engram_ml::Embedder;
        let temporary = tempfile::tempdir()?;
        let result = async {
            let root = temporary.path().join("repo");
            std::fs::create_dir(&root)?;
            std::fs::write(root.join("helper.rs"), "pub fn authorize_save() {}\n")?;
            let repo = Repository::init(&root)?;
            let mut index = repo.index()?;
            index.add_path(Path::new("helper.rs"))?;
            let tree = repo.find_tree(index.write_tree()?)?;
            let signature = git2::Signature::now("Fixture", "fixture@example.invalid")?;
            let tip = repo.commit(
                Some("HEAD"),
                &signature,
                &signature,
                "Implement scoped save authorization\n\nKeep authorization before saving.",
                &tree,
                &[],
            )?;
            let cfg_a = engram_core::Config {
                data_dir: temporary.path().join("data-a"),
                embedding_backend: "local".into(),
                embedding_model: None,
                ..Default::default()
            };
            let cfg_b = engram_core::Config {
                data_dir: temporary.path().join("data-b"),
                embedding_model: Some("nomic-fixture".into()),
                ..cfg_a.clone()
            };
            let engine_a = engram_index::HybridSearchEngine::new(
                temporary.path().join("search-a/tantivy"),
                temporary.path().join("search-a/lance"),
                &cfg_a,
            )
            .await?;
            let engine_b = engram_index::HybridSearchEngine::new(
                temporary.path().join("search-b/tantivy"),
                temporary.path().join("search-b/lance"),
                &cfg_b,
            )
            .await?;
            let prefixes_a = EmbedPrefixes::for_model(cfg_a.embedding_model.as_deref());
            let prefixes_b = EmbedPrefixes::for_model(cfg_b.embedding_model.as_deref());
            let provider = engram_ml::embed::LocalEmbedder;
            let mut observed = Vec::new();
            for (engine, prefixes) in [
                (&engine_a, prefixes_a),
                (&engine_b, prefixes_b),
                (&engine_a, prefixes_a),
            ] {
                let cards = cards_for_line(&root, tip, engine, prefixes).await?;
                anyhow::ensure!(cards.len() == 1 && cards[0].oid == tip.to_string());
                let expected = provider
                    .embed(&format!("{}{}", prefixes.document, cards[0].text()))
                    .await?;
                anyhow::ensure!(
                    cards[0].vector == expected,
                    "history reused another configuration's vectors"
                );
                observed.push(cards[0].vector.clone());
            }
            anyhow::ensure!(
                observed[0] != observed[1],
                "fixture must exercise distinct document inputs"
            );
            anyhow::ensure!(observed[0] == observed[2]);
            let profiles = [
                "profile0: ",
                "profile1: ",
                "profile2: ",
                "profile3: ",
                "profile4: ",
                "profile5: ",
                "profile6: ",
                "profile7: ",
                "profile8: ",
                "profile9: ",
            ];
            let requests = profiles.iter().map(|&document| {
                cards_for_line(
                    &root,
                    tip,
                    &engine_a,
                    EmbedPrefixes {
                        document,
                        query: "",
                    },
                )
            });
            let completed = futures::future::join_all(requests).await;
            for (document, cards) in profiles.into_iter().zip(completed) {
                let cards = cards?;
                anyhow::ensure!(cards.len() == 1 && cards[0].oid == tip.to_string());
                let expected = provider
                    .embed(&format!("{document}{}", cards[0].text()))
                    .await?;
                anyhow::ensure!(
                    cards[0].vector == expected,
                    "concurrent requests or cache pressure changed the requested history vectors"
                );
            }
            Ok(())
        }
        .await;
        temporary.close()?;
        result
    }

    struct TestLlm {
        reply: Option<&'static str>,
        dropped: AtomicBool,
    }

    #[async_trait::async_trait]
    impl LlmProvider for TestLlm {
        fn name(&self) -> &'static str {
            "budget-test"
        }

        async fn generate(&self, _: &str, _: LlmGenerateOptions) -> Result<LlmResponse, LlmError> {
            struct Finish<'a>(&'a AtomicBool);
            impl Drop for Finish<'_> {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::SeqCst);
                }
            }
            let _finish = Finish(&self.dropped);
            match self.reply {
                Some("provider-error") => Err(LlmError::Transport {
                    provider: Some("budget-test".into()),
                    status_code: None,
                    retry_exhausted: true,
                    message: "fixture unavailable".into(),
                }),
                Some(text) => Ok(LlmResponse { text: text.into() }),
                None => std::future::pending().await,
            }
        }
    }

    fn budget_cards(count: usize) -> Vec<Arc<ChangeCard>> {
        (0..count)
            .map(|i| card(&format!("{i:040x}"), 1, vec![1.0]))
            .collect()
    }

    fn ids(cards: &[Arc<ChangeCard>]) -> Vec<String> {
        cards.iter().map(|c| c.oid.clone()).collect()
    }

    #[tokio::test]
    async fn skipped_or_invalid_budget_never_constructs_provider() {
        let calls = AtomicUsize::new(0);
        for (enabled, budget, count, expected) in [
            (false, 90_000, 2, RerankOutcome::Disabled),
            (true, 0, 2, RerankOutcome::ZeroBudget),
            (true, 90_000, 0, RerankOutcome::NoCandidates),
        ] {
            let baseline = budget_cards(count);
            let expected_ids = ids(&baseline);
            let result = rerank_with_budget("story", baseline, enabled, budget, || {
                calls.fetch_add(1, Ordering::SeqCst);
                None
            })
            .await
            .unwrap();
            assert_eq!(result.outcome, expected);
            assert_eq!(ids(&result.ranked), expected_ids);
        }
        assert!(
            rerank_with_budget("story", budget_cards(2), true, 90_001, || {
                calls.fetch_add(1, Ordering::SeqCst);
                None
            })
            .await
            .is_err()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(checked_rerank_timeout_ms(None).unwrap(), 90_000);
    }

    #[tokio::test]
    async fn deadline_retains_baseline_and_drops_pending_provider_future() {
        let probe = Arc::new(TestLlm {
            reply: None,
            dropped: AtomicBool::new(false),
        });
        let provider: Arc<dyn LlmProvider> = probe.clone();
        let baseline = budget_cards(32);
        let expected = ids(&baseline);
        let result = rerank_with_budget("story", baseline, true, 10, move || {
            Some((provider, "fixture-model".into()))
        })
        .await
        .unwrap();
        assert_eq!(ids(&result.ranked), expected);
        assert_eq!(result.outcome, RerankOutcome::TimedOut { budget_ms: 10 });
        assert!(probe.dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn invalid_picks_keep_baseline_but_valid_partial_order_keeps_tail() {
        for (reply, succeeds) in [
            (r#"{"best":[2,1]}"#, true),
            (r#"{"best":[]}"#, true),
            (r#"{"best":[1,1]}"#, false),
            (r#"{"best":[0]}"#, false),
            (r#"{"best":[31]}"#, false),
            (r#"{"best":[1.5]}"#, false),
            (r#"{"best":[1,2,3,4,5,6]}"#, false),
            ("not JSON", false),
            ("provider-error", false),
        ] {
            let provider: Arc<dyn LlmProvider> = Arc::new(TestLlm {
                reply: Some(reply),
                dropped: AtomicBool::new(false),
            });
            let baseline = budget_cards(32);
            let mut expected = ids(&baseline);
            if reply == r#"{"best":[2,1]}"# {
                expected.swap(0, 1);
            }
            let result = rerank_with_budget("story", baseline, true, 90_000, move || {
                Some((provider, "fixture-model".into()))
            })
            .await
            .unwrap();
            assert_eq!(ids(&result.ranked), expected, "reply: {reply}");
            assert_eq!(
                matches!(
                    result.outcome,
                    RerankOutcome::Completed { pool_size: 30, .. }
                ),
                succeeds
            );
            if !succeeds {
                assert!(matches!(result.outcome, RerankOutcome::Failed { .. }));
            }
        }
        let baseline = budget_cards(32);
        let expected = ids(&baseline);
        let result = rerank_with_budget("story", baseline, true, 90_000, || None)
            .await
            .unwrap();
        assert_eq!(result.outcome, RerankOutcome::Unavailable);
        assert_eq!(ids(&result.ranked), expected);
    }

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
