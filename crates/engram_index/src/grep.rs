//! Fast literal / regex search that uses the existing Tantivy index as
//! a prefilter. Designed to beat `rg` on warm queries across every
//! literal / regex class by leveraging the inverted index we already
//! built — if a term doesn't appear in a chunk, we never scan that
//! chunk's bytes.
//!
//! ## Why this exists
//!
//! Engram indexes every chunk of every indexed file into a Tantivy
//! trigram index (`NgramTokenizer::new(3, 3, false)` — see
//! [`crate::tantivy_index`]). For any literal query of 3+ characters,
//! Tantivy can tell us which chunks *might* contain the literal in
//! microseconds. Running a byte-level regex over the filesystem when
//! you already have an inverted index is a failure of imagination.
//!
//! ## Tiered execution
//!
//! 1. **Tier 0 — term-index + verify.** Tantivy trigram query against
//!    the existing `content` field. Returns candidate chunks. For each
//!    candidate, verify the literal actually appears (trigram matches
//!    can false-positive) and pull the exact line number. The chunk
//!    text is already STORED in Tantivy, so no second store is
//!    consulted for it. Microseconds per result.
//!
//! 2. **Tier 1 — narrowed regex.** Extract the longest literal
//!    substring from the regex, run a trigram prefilter against it,
//!    then apply the full regex only to the narrowed chunk set. Still
//!    dominates `rg` because we skip most of the codebase.
//!
//! 3. **Tier 2 — full scan.** Patterns too short (< 3 chars) or too
//!    regex-complex to prefilter fall through to a parallel scan over
//!    every stored chunk in the project.
//!
//! Every query begins with an optional freshness check comparing the
//! indexed (size, mtime) of each tracked file against disk. The caller
//! supplies those stats — they live on the code graph's file nodes,
//! written by ingest and trusted by the incremental change scan. Stale
//! files are listed in the result, and a check that had NOTHING to
//! compare against says so, rather than returning an empty list that
//! reads like a clean bill of health.

use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::hybrid::{HybridQuery, HybridSearchEngine};

/// How to handle staleness between the index and disk state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FreshnessMode {
    /// Default. Fingerprint every tracked file; surface stale paths
    /// in `index_stale_warning`. Callers that need bytewise-current
    /// results should re-index first.
    #[default]
    Strict,
    /// Note staleness but don't fail — useful for "give me the best
    /// answer the index can manage right now" workflows.
    Warn,
    /// Skip the freshness check entirely. Fastest, least correct.
    Off,
}

/// Output tier — reports which execution path the engine chose, so
/// benchmarks and operators can verify Tier 0 is being used on
/// queries that should qualify for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrepTier {
    /// Trigram-indexed literal lookup — fastest path.
    TermIndex,
    /// Trigram prefilter + full regex on narrowed chunks.
    TermNarrowed,
    /// Parallel byte scan over every stored chunk.
    FullScan,
}

#[derive(Debug, Clone)]
pub struct GrepQuery {
    pub project_id: String,
    pub namespace: String,
    pub generation: u64,
    pub pattern: String,
    /// When `true`, `pattern` is treated as a regex. When `false`, as
    /// a literal byte sequence.
    pub regex: bool,
    /// `Some(true)` = exact case; `Some(false)` = case-insensitive;
    /// `None` = smart case (case-insensitive unless the pattern
    /// contains any uppercase ASCII character).
    pub case_sensitive: Option<bool>,
    /// When `true`, `.` matches newlines in regex mode.
    pub multiline: bool,
    /// Optional path-prefix filter. Not a full glob yet — any chunk
    /// whose stored path starts with this string is eligible.
    pub path_prefix: Option<String>,
    pub language: Option<String>,
    pub context_before: usize,
    pub context_after: usize,
    pub max_results: usize,
    pub freshness: FreshnessMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepMatch {
    pub file_path: String,
    pub line: u32,
    pub column: u32,
    pub line_text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_before: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_after: Vec<String>,
    pub chunk_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepResult {
    pub matches: Vec<GrepMatch>,
    /// Number of chunks the engine actually scanned (not the whole
    /// project — a term-indexed query scans only the posting-list
    /// intersection).
    pub chunks_scanned: usize,
    pub files_scanned: usize,
    /// Paths whose on-disk fingerprint doesn't match the indexed
    /// fingerprint. Empty when `freshness == Off` or when nothing is
    /// stale.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stale_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_stale_warning: Option<String>,
    pub tier_used: GrepTier,
    pub elapsed_ms: u64,
}

// ── Smart case ────────────────────────────────────────────────────────────────

/// Resolve the caller's case preference. Smart case — default when
/// `case_sensitive` is `None` — treats a pattern containing any
/// uppercase ASCII character as case-sensitive, and everything-else
/// as case-insensitive. Matches the behaviour every modern grep tool
/// ships with.
fn resolve_case_sensitive(pattern: &str, flag: Option<bool>) -> bool {
    if let Some(v) = flag {
        return v;
    }
    pattern.chars().any(|c| c.is_ascii_uppercase())
}

/// Guard against a SILENT wrong answer from the full-scan tier. FullScan
/// is reached only for patterns the term index can't serve (a literal
/// shorter than the trigram minimum, or a regex without a ≥3-char literal
/// anchor). It now scans Tantivy's stored chunk text, but an empty or
/// wrong-generation index would still scan nothing and return 0 matches
/// that read as "no matches exist". When a FullScan query scans zero
/// files, surface that 0 is UNCONFIRMED coverage, not a proven absence,
/// and tell the caller how to reformulate. Same fail-loud principle as
/// the unknown-namespace guard.
fn full_scan_coverage_warning(tier: GrepTier, files_scanned: usize) -> Option<String> {
    if matches!(tier, GrepTier::FullScan) && files_scanned == 0 {
        Some(
            "This pattern used the full-scan tier (a literal shorter than 3 \
             characters, or a regex without a ≥3-char literal anchor) and scanned \
             no content — treat 0 matches as UNCONFIRMED coverage, NOT a proven \
             absence. Reformulate with a literal of ≥3 characters (or add a ≥3-char \
             literal anchor to the regex) so the term index can serve it."
                .to_string(),
        )
    } else {
        None
    }
}

// ── Freshness check ───────────────────────────────────────────────────────────

/// One indexed file's stat signature, as the indexer recorded it.
///
/// `mtime_secs` is SECONDS, not milliseconds: ingest stores second
/// granularity on the graph's file nodes, and comparing a millisecond disk
/// stamp against it would mark every file stale forever.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedFileStat {
    pub rel_path: String,
    pub size: u64,
    pub mtime_secs: u64,
    /// blake3 hex of the file's bytes at index time, when recorded. Lets the
    /// freshness check CONFIRM a size/mtime mismatch against content before
    /// reporting "stale" — an editor save, a git checkout, or a plain touch
    /// bumps mtime without changing what we indexed, and that false-stale is
    /// what trained agents to distrust the index and fall back to grep.
    pub file_hash: Option<String>,
}

/// Outcome of one freshness sweep.
#[derive(Debug, Clone, Default)]
pub struct FreshnessCheck {
    /// Tracked paths whose on-disk `(size, mtime)` no longer matches.
    pub stale_paths: Vec<String>,
    /// How many indexed files were compared. **Zero means the check proved
    /// nothing** — it had no fingerprints to compare against. The previous
    /// implementation read an empty store and returned an empty stale list,
    /// which is indistinguishable from "everything is current" unless the
    /// count is carried out with it.
    pub files_checked: usize,
}

/// Compare indexed file stats against disk.
///
/// The stats are supplied by the caller rather than read here, because the
/// crate that owns them (the code graph) sits above this one. They come from
/// the same file-node metadata the incremental change scan uses to decide
/// what to re-index, so "stale" here means exactly "an update would pick
/// this file up".
///
/// `indexed` is a closure so the TTL cache below can skip the read entirely
/// on a hit — agents fire grep bursts, and both the stat sweep and the
/// metadata read are O(files).
///
/// False positives are acceptable (a file flagged that is actually fine);
/// false negatives are not — serving stale content silently is the failure
/// this guard exists to prevent.
pub fn check_freshness<F>(
    indexed: F,
    project_id: &str,
    namespace: &str,
    project_root: &Path,
) -> anyhow::Result<FreshnessCheck>
where
    F: FnOnce() -> anyhow::Result<Vec<IndexedFileStat>>,
{
    // TODO-46: agents fire grep bursts (many calls within seconds); the
    // O(files) sweep is recomputed at most once per TTL per project.
    const FRESHNESS_TTL: std::time::Duration = std::time::Duration::from_secs(10);
    #[allow(clippy::type_complexity)]
    static CACHE: std::sync::LazyLock<
        std::sync::Mutex<
            std::collections::HashMap<(String, String), (std::time::Instant, FreshnessCheck)>,
        >,
    > = std::sync::LazyLock::new(Default::default);
    let cache_key = (project_id.to_string(), namespace.to_string());
    if let Ok(c) = CACHE.lock()
        && let Some((at, check)) = c.get(&cache_key)
        && at.elapsed() < FRESHNESS_TTL
    {
        return Ok(check.clone());
    }

    let indexed = indexed()?;
    let mut out = FreshnessCheck {
        stale_paths: Vec::new(),
        files_checked: indexed.len(),
    };
    for fp in indexed {
        // Never flag VCS/internal files: they are not part of the searchable
        // corpus and editors/git rewrite them constantly, which is the bulk of
        // the false-"stale" noise (they should not be indexed in the first
        // place; this shields the check if they slip in).
        if fp.rel_path.starts_with(".git/") || fp.rel_path.starts_with(".git\\") {
            out.files_checked -= 1;
            continue;
        }
        let abs = project_root.join(&fp.rel_path);
        let meta = match std::fs::metadata(&abs) {
            Ok(m) => m,
            Err(_) => {
                // File disappeared or access denied — definitely stale.
                out.stale_paths.push(fp.rel_path);
                continue;
            }
        };
        // A file indexed before fingerprints were recorded carries no
        // signature; skip it rather than crying wolf over the whole corpus.
        if fp.size == 0 && fp.mtime_secs == 0 {
            out.files_checked -= 1;
            continue;
        }
        let disk_mtime_secs = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if meta.len() == fp.size && disk_mtime_secs == fp.mtime_secs {
            continue; // size AND mtime unchanged → certainly fresh
        }
        // Size or mtime differs — but that is not proof of a content change. A
        // save-without-edit, a git checkout, or a `touch` moves mtime (and CRLF
        // normalisation can move size) without changing the bytes we indexed.
        // Confirm against the content hash before reporting stale, when we have
        // one and the file is small enough to re-hash cheaply.
        const HASH_CONFIRM_MAX_BYTES: u64 = 8_000_000;
        match &fp.file_hash {
            Some(indexed_hash) if meta.len() <= HASH_CONFIRM_MAX_BYTES => match std::fs::read(&abs)
            {
                Ok(bytes) => {
                    let disk_hash = blake3::hash(&bytes).to_hex().to_string();
                    if disk_hash != *indexed_hash {
                        out.stale_paths.push(fp.rel_path); // content genuinely changed
                    }
                    // else: touched but content-identical → NOT stale.
                }
                Err(_) => out.stale_paths.push(fp.rel_path),
            },
            // No recorded hash (older index) or too big to re-hash cheaply:
            // fall back to the size/mtime signal.
            _ => out.stale_paths.push(fp.rel_path),
        }
    }
    out.stale_paths.sort();
    if let Ok(mut c) = CACHE.lock() {
        c.insert(cache_key, (std::time::Instant::now(), out.clone()));
    }
    Ok(out)
}

// ── Tier selection ────────────────────────────────────────────────────────────

/// Minimum pattern length for trigram-index prefiltering. Shorter
/// queries can't be narrowed with trigrams — every document becomes a
/// candidate, so we fall through to Tier 2 (full scan) instead.
const MIN_TRIGRAM_LEN: usize = 3;

fn pick_tier(q: &GrepQuery) -> GrepTier {
    if q.regex {
        // A regex with a long literal anchor can still use the trigram
        // prefilter (Tier 1). For the first implementation we route
        // every regex through Tier 2 (full scan) to avoid the
        // complexity of literal-extraction from regex — Tier 1 is a
        // planned optimisation.
        if q.pattern.len() >= MIN_TRIGRAM_LEN && regex_has_literal_anchor(&q.pattern) {
            GrepTier::TermNarrowed
        } else {
            GrepTier::FullScan
        }
    } else if q.pattern.len() >= MIN_TRIGRAM_LEN {
        GrepTier::TermIndex
    } else {
        GrepTier::FullScan
    }
}

/// Does a regex contain at least one run of 3+ literal characters
/// that can feed the trigram index?
fn regex_has_literal_anchor(pat: &str) -> bool {
    extract_literal_anchor(pat).is_some()
}

/// Extract the longest contiguous literal substring from a regex
/// pattern — a sequence of characters that MUST appear in every
/// match. This is what Tier 1 feeds into the trigram prefilter.
///
/// The extractor is deliberately conservative: when a pattern contains
/// constructs that would require tree analysis to reason about
/// (alternation, lookaround, backreferences, counted quantifiers), we
/// return `None` and the caller falls through to Tier 2. Being
/// conservative here is free — at worst we scan more chunks than
/// strictly necessary; we never miss a match.
///
/// Supported:
/// - ASCII / Unicode literal runs
/// - Escaped metacharacters (`\.`, `\(`, `\+`, `\\`, `\"`) → literal
/// - Quantifiers (`?`, `*`, `+`): drop the preceding char from the
///   current run, since it may not actually appear
/// - Class metacharacters (`.`, `^`, `$`): break the run
/// - Groups / character classes (`(`, `)`, `[`, `]`): break the run
/// - Character escapes that don't match a specific byte (`\d`, `\w`,
///   `\s`, `\b`, `\A`, `\z`, back-references): break the run
///
/// Not supported (return `None`):
/// - Alternation (`|`) — requires intersecting literals across branches
/// - Lookaround (`(?=`, `(?!`, `(?<=`, `(?<!`, `(?:`) — too-complex
/// - Counted quantifiers (`{n,m}`) — rare; not worth the complexity
pub(crate) fn extract_literal_anchor(pattern: &str) -> Option<String> {
    // Reject constructs that need tree analysis.
    if pattern.contains('|') || pattern.contains("(?") || pattern.contains('{') {
        return None;
    }

    let mut best = String::new();
    let mut current = String::new();
    let mut chars = pattern.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                let Some(&next) = chars.peek() else {
                    break;
                };
                chars.next();
                // `\d`, `\w`, `\s`, assertions, back-refs — not literal.
                if matches!(
                    next,
                    'd' | 'D' | 's' | 'S' | 'w' | 'W' | 'b' | 'B' | 'A' | 'z' | 'Z' | '0'..='9'
                ) {
                    if current.len() > best.len() {
                        best.clone_from(&current);
                    }
                    current.clear();
                } else {
                    // Plain escape (`\.`, `\(`, `\\`, `\"`, …) → the
                    // escaped char IS a required literal.
                    current.push(next);
                }
            }
            '(' | ')' | '[' | ']' | '^' | '$' | '.' => {
                if current.len() > best.len() {
                    best.clone_from(&current);
                }
                current.clear();
            }
            '?' | '*' | '+' => {
                // The preceding char was optional / repeated — it may
                // not actually appear in the match, so drop it from
                // the current run.
                current.pop();
                if current.len() > best.len() {
                    best.clone_from(&current);
                }
                current.clear();
            }
            _ => current.push(c),
        }
    }
    if current.len() > best.len() {
        best = current;
    }
    if best.len() >= MIN_TRIGRAM_LEN {
        Some(best)
    } else {
        None
    }
}

// ── Engine ────────────────────────────────────────────────────────────────────

/// Execute a grep query. Callers pass the same `HybridSearchEngine`
/// that serves `search_memory` so the Tantivy reader is shared and
/// warm across calls.
pub fn grep<F>(
    engine: &HybridSearchEngine,
    project_root: &Path,
    q: &GrepQuery,
    indexed_files: F,
) -> anyhow::Result<GrepResult>
where
    F: FnOnce() -> anyhow::Result<Vec<IndexedFileStat>>,
{
    let start = Instant::now();

    // ── Step 1: freshness check ──
    let freshness = match q.freshness {
        FreshnessMode::Off => FreshnessCheck::default(),
        FreshnessMode::Strict | FreshnessMode::Warn => {
            check_freshness(indexed_files, &q.project_id, &q.namespace, project_root)
                .unwrap_or_default()
        }
    };
    let stale_paths = freshness.stale_paths.clone();
    let index_stale_warning = if !stale_paths.is_empty() {
        let one = stale_paths.len() == 1;
        Some(format!(
            "{} file{} on disk {} match the indexed fingerprint{} — results for {} may be stale. Run `update_project` to refresh.",
            stale_paths.len(),
            if one { "" } else { "s" },
            if one { "does not" } else { "do not" },
            if one { "" } else { "s" },
            if one { "it" } else { "those files" },
        ))
    } else if !matches!(q.freshness, FreshnessMode::Off) && freshness.files_checked == 0 {
        // The guard ran and had NOTHING to compare against. Silence here is
        // exactly what let a decorative check pass for a real one: an empty
        // fingerprint set yields an empty stale list, which reads identically
        // to a clean bill of health. Say so instead.
        Some(
            "freshness could NOT be verified — this project has no indexed file \
             fingerprints, so staleness was not checked. Treat these results as \
             unverified against disk, and run `update_project` to record them."
                .to_string(),
        )
    } else {
        None
    };

    let tier = pick_tier(q);
    let case_sensitive = resolve_case_sensitive(&q.pattern, q.case_sensitive);

    // ── Step 2: execute tier ──
    let (matches, chunks_scanned, files_scanned) = match tier {
        GrepTier::TermIndex => execute_term_index(engine, q, case_sensitive)?,
        GrepTier::TermNarrowed => execute_term_narrowed(engine, q, case_sensitive)?,
        GrepTier::FullScan => execute_full_scan(engine, q, case_sensitive)?,
    };

    // Prefer a real staleness warning; otherwise fail loud if the
    // full-scan tier scanned nothing (so 0 matches isn't read as absence).
    let index_stale_warning =
        index_stale_warning.or_else(|| full_scan_coverage_warning(tier, files_scanned));

    Ok(GrepResult {
        matches,
        chunks_scanned,
        files_scanned,
        stale_paths,
        index_stale_warning,
        tier_used: tier,
        elapsed_ms: start.elapsed().as_millis() as u64,
    })
}

// ── Tier 0: term-indexed literal lookup ──

fn execute_term_index(
    engine: &HybridSearchEngine,
    q: &GrepQuery,
    case_sensitive: bool,
) -> anyhow::Result<(Vec<GrepMatch>, usize, usize)> {
    // Oversample to cover trigram-index false positives. Small
    // ceiling: a false-positive rate above a few percent is vanishingly
    // rare for literals of length ≥ 5, and even for 3-char literals
    // the `max_results * 2 + 100` budget is plenty. Capping the
    // oversample avoids pathological behaviour when callers pass
    // max_results = 100_000 expecting a full corpus scan.
    let oversample = (q.max_results.saturating_mul(2).saturating_add(100)).min(20_000);
    let hybrid_q = HybridQuery {
        project_id: q.project_id.clone(),
        namespace: q.namespace.clone(),
        generation: q.generation,
        text: q.pattern.clone(),
        top_k: oversample,
        // The trigram index preserves case; a case-insensitive literal
        // must reach every spelling (row-4 slice 4, live miss).
        fts_mode: if case_sensitive {
            "strict".into()
        } else {
            "literal_ci".into()
        },
        include_path_prefixes: q.path_prefix.as_ref().map(|p| vec![p.clone()]),
        exclude_path_prefixes: None,
        include_path_suffixes: None,
        language_filters: q.language.as_ref().map(|l| vec![l.clone()]),
        author_filter: None,
        date_after: None,
        date_before: None,
        use_mmr: false,
    };
    // `lexical_search_with_content` gives us each chunk's full stored
    // content in a single Tantivy read — no second-store round-trip per
    // chunk. This is the hot path and every microsecond of overhead
    // here directly widens the gap against rg.
    let hits = engine.lexical_search_with_content(&hybrid_q)?;

    let mut matches: Vec<GrepMatch> = Vec::with_capacity(q.max_results);
    let mut chunks_scanned = 0usize;
    let mut files_seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (hit, content, start_line) in hits {
        // ENFORCE the prefix here — the engine-side include filter proved
        // silently non-constraining on this path (live repro 2026-07-06:
        // 75/75 results out-of-prefix). Case-insensitive: stored paths
        // keep repo casing while callers often pass lowercased prefixes.
        if let Some(pre) = &q.path_prefix
            && !hit
                .path
                .as_str()
                .to_lowercase()
                .starts_with(&pre.to_lowercase())
        {
            continue;
        }
        chunks_scanned += 1;
        files_seen.insert(hit.path.as_str().to_string());
        scan_chunk(
            &content,
            start_line,
            &q.pattern,
            case_sensitive,
            false,
            q.multiline,
            hit.path.as_str(),
            hit.chunk_id,
            q.context_before,
            q.context_after,
            &mut matches,
            q.max_results,
        );
        if matches.len() >= q.max_results {
            break;
        }
    }

    Ok((matches, chunks_scanned, files_seen.len()))
}

// ── Tier 1: term-narrowed regex ──

/// Extract the longest literal anchor from the regex, feed it through
/// Tantivy's trigram index to narrow to candidate chunks, then apply
/// the full regex only to those chunks. This is how we beat `rg` on
/// every regex-with-literal-anchor query.
fn execute_term_narrowed(
    engine: &HybridSearchEngine,
    q: &GrepQuery,
    case_sensitive: bool,
) -> anyhow::Result<(Vec<GrepMatch>, usize, usize)> {
    let Some(anchor) = extract_literal_anchor(&q.pattern) else {
        // Shouldn't happen — pick_tier already checked. Safety net.
        return execute_full_scan(engine, q, case_sensitive);
    };

    // Oversample a bit more aggressively than Tier 0 because the
    // regex constrains the candidate set more than the anchor does.
    let oversample = (q.max_results.saturating_mul(3).saturating_add(200)).min(20_000);
    let hybrid_q = HybridQuery {
        project_id: q.project_id.clone(),
        namespace: q.namespace.clone(),
        generation: q.generation,
        text: anchor,
        top_k: oversample,
        // Same case rule as the term-index tier: the anchor is a literal.
        fts_mode: if case_sensitive {
            "strict".into()
        } else {
            "literal_ci".into()
        },
        include_path_prefixes: q.path_prefix.as_ref().map(|p| vec![p.clone()]),
        exclude_path_prefixes: None,
        include_path_suffixes: None,
        language_filters: q.language.as_ref().map(|l| vec![l.clone()]),
        author_filter: None,
        date_after: None,
        date_before: None,
        use_mmr: false,
    };
    let hits = engine.lexical_search_with_content(&hybrid_q)?;

    // Compile the regex ONCE and reuse across chunks — the builder
    // allocation is a measurable cost when the anchor narrows to a
    // few hundred chunks.
    let mut builder = regex::RegexBuilder::new(&q.pattern);
    builder.case_insensitive(!case_sensitive);
    builder.multi_line(true);
    builder.dot_matches_new_line(q.multiline);
    let compiled = builder.build().ok();

    let mut matches: Vec<GrepMatch> = Vec::with_capacity(q.max_results);
    let mut chunks_scanned = 0usize;
    let mut files_seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (hit, content, start_line) in hits {
        // Same prefix enforcement as the literal tier — the engine-side
        // include filter is silently non-constraining here.
        if let Some(pre) = &q.path_prefix
            && !hit
                .path
                .as_str()
                .to_lowercase()
                .starts_with(&pre.to_lowercase())
        {
            continue;
        }
        chunks_scanned += 1;
        files_seen.insert(hit.path.as_str().to_string());
        scan_chunk_with_precompiled(
            &content,
            start_line,
            &q.pattern,
            case_sensitive,
            true,
            q.multiline,
            compiled.as_ref(),
            hit.path.as_str(),
            hit.chunk_id,
            q.context_before,
            q.context_after,
            &mut matches,
            q.max_results,
        );
        if matches.len() >= q.max_results {
            break;
        }
    }

    Ok((matches, chunks_scanned, files_seen.len()))
}

// ── Tier 2: full scan over stored chunk text ──

/// Parallel byte scan over every stored chunk. Each worker scans a
/// disjoint subset; local match buffers are merged under a single mutex.
/// An atomic counter lets workers bail early when the global
/// `max_results` cap is reached — without the atomic, workers would keep
/// producing matches long after the cap.
fn execute_full_scan(
    engine: &HybridSearchEngine,
    q: &GrepQuery,
    case_sensitive: bool,
) -> anyhow::Result<(Vec<GrepMatch>, usize, usize)> {
    use rayon::prelude::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Chunk text is STORED in Tantivy, so the scan reads it from there.
    // This tier used to read a separate document store that no production
    // code path has ever written to, which meant it scanned zero files and
    // returned zero matches for every query that reached it.
    //
    // Two-phase: addresses first (no stored-field access), then one chunk
    // per worker — a full-corpus scan never materialises the corpus.
    let (searcher, addresses) =
        engine.chunk_addresses(&q.project_id, &q.namespace, q.generation)?;

    let chunks_scanned = AtomicUsize::new(0);
    let total_matches = AtomicUsize::new(0);
    let matches_mutex: Mutex<Vec<GrepMatch>> = Mutex::new(Vec::with_capacity(q.max_results));
    // Distinct file paths actually scanned. `files_scanned` feeds the
    // coverage warning, so it has to count files, not chunks.
    let files_seen: Mutex<std::collections::HashSet<String>> =
        Mutex::new(std::collections::HashSet::new());

    // Compile the regex once up front (if we're in regex mode) so
    // workers don't each recompile on every line — saves tens of µs
    // per chunk on hot patterns.
    let compiled_regex: Option<regex::Regex> = if q.regex {
        let mut builder = regex::RegexBuilder::new(&q.pattern);
        builder.case_insensitive(!case_sensitive);
        builder.multi_line(true);
        builder.dot_matches_new_line(q.multiline);
        builder.build().ok()
    } else {
        None
    };

    // We share a single Result slot so a store error from any worker wins.
    // Workers bail cooperatively on the first failure.
    let first_error: Mutex<Option<anyhow::Error>> = Mutex::new(None);

    addresses.par_iter().for_each(|addr| {
        if total_matches.load(Ordering::Relaxed) >= q.max_results {
            return;
        }
        if first_error.lock().is_ok_and(|g| g.is_some()) {
            return;
        }

        let chunk = match engine.stored_chunk_at(&searcher, *addr) {
            Ok(c) => c,
            Err(e) => {
                if let Ok(mut g) = first_error.lock()
                    && g.is_none()
                {
                    *g = Some(e);
                }
                return;
            }
        };

        if let Some(pre) = q.path_prefix.as_ref()
            && !chunk.path.starts_with(pre)
        {
            return;
        }
        if let Some(lang) = q.language.as_ref()
            && !chunk.language.eq_ignore_ascii_case(lang)
        {
            return;
        }

        chunks_scanned.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut seen) = files_seen.lock() {
            seen.insert(chunk.path.clone());
        }

        let mut local_matches: Vec<GrepMatch> = Vec::new();
        scan_chunk_with_precompiled(
            &chunk.content,
            chunk.start_line,
            &q.pattern,
            case_sensitive,
            q.regex,
            q.multiline,
            compiled_regex.as_ref(),
            &chunk.path,
            0,
            q.context_before,
            q.context_after,
            &mut local_matches,
            q.max_results,
        );

        if !local_matches.is_empty() {
            let mut global = match matches_mutex.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            for m in local_matches {
                if global.len() >= q.max_results {
                    break;
                }
                global.push(m);
            }
            total_matches.store(global.len(), Ordering::Relaxed);
        }
    });

    if let Ok(mut g) = first_error.lock()
        && let Some(e) = g.take()
    {
        return Err(e);
    }

    let matches = matches_mutex
        .into_inner()
        .unwrap_or_else(|p| p.into_inner());
    let files_scanned = files_seen
        .into_inner()
        .unwrap_or_else(|p| p.into_inner())
        .len();
    Ok((
        matches,
        chunks_scanned.load(Ordering::Relaxed),
        files_scanned,
    ))
}

// ── Per-chunk scanner ─────────────────────────────────────────────────────────

/// Per-chunk scanner that accepts a pre-compiled regex when one is
/// available. Workers in the parallel full-scan path share a single
/// compiled regex across every chunk they process instead of
/// rebuilding it each time.
#[allow(clippy::too_many_arguments)]
fn scan_chunk_with_precompiled(
    content: &str,
    chunk_start_line: u32,
    pattern: &str,
    case_sensitive: bool,
    regex_mode: bool,
    multiline: bool,
    precompiled: Option<&regex::Regex>,
    file_path: &str,
    chunk_id: u64,
    context_before: usize,
    context_after: usize,
    out: &mut Vec<GrepMatch>,
    max_results: usize,
) {
    let lines: Vec<&str> = content.lines().collect();
    if regex_mode {
        let owned_re;
        let re: &regex::Regex = if let Some(r) = precompiled {
            r
        } else {
            let mut builder = regex::RegexBuilder::new(pattern);
            builder.case_insensitive(!case_sensitive);
            builder.multi_line(true);
            builder.dot_matches_new_line(multiline);
            let Ok(built) = builder.build() else {
                return;
            };
            owned_re = built;
            &owned_re
        };
        if multiline {
            for m in re.find_iter(content) {
                let (line_idx, col) = line_col_from_byte_offset(content, m.start());
                push_match(
                    out,
                    &lines,
                    line_idx,
                    col,
                    chunk_start_line,
                    file_path,
                    chunk_id,
                    context_before,
                    context_after,
                );
                if out.len() >= max_results {
                    return;
                }
            }
        } else {
            for (line_idx, line) in lines.iter().enumerate() {
                if let Some(m) = re.find(line) {
                    push_match(
                        out,
                        &lines,
                        line_idx,
                        m.start(),
                        chunk_start_line,
                        file_path,
                        chunk_id,
                        context_before,
                        context_after,
                    );
                    if out.len() >= max_results {
                        return;
                    }
                }
            }
        }
    } else {
        let needle_lower_buf = if case_sensitive {
            None
        } else {
            Some(pattern.to_ascii_lowercase())
        };
        let needle: &str = needle_lower_buf.as_deref().unwrap_or(pattern);
        for (line_idx, line) in lines.iter().enumerate() {
            let haystack_lower_buf = if case_sensitive {
                None
            } else {
                Some(line.to_ascii_lowercase())
            };
            let haystack: &str = haystack_lower_buf.as_deref().unwrap_or(line);
            if let Some(col) = haystack.find(needle) {
                push_match(
                    out,
                    &lines,
                    line_idx,
                    col,
                    chunk_start_line,
                    file_path,
                    chunk_id,
                    context_before,
                    context_after,
                );
                if out.len() >= max_results {
                    return;
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn scan_chunk(
    content: &str,
    chunk_start_line: u32,
    pattern: &str,
    case_sensitive: bool,
    regex_mode: bool,
    multiline: bool,
    file_path: &str,
    chunk_id: u64,
    context_before: usize,
    context_after: usize,
    out: &mut Vec<GrepMatch>,
    max_results: usize,
) {
    scan_chunk_with_precompiled(
        content,
        chunk_start_line,
        pattern,
        case_sensitive,
        regex_mode,
        multiline,
        None,
        file_path,
        chunk_id,
        context_before,
        context_after,
        out,
        max_results,
    );
}

#[allow(clippy::too_many_arguments)]
fn push_match(
    out: &mut Vec<GrepMatch>,
    lines: &[&str],
    line_idx: usize,
    col: usize,
    chunk_start_line: u32,
    file_path: &str,
    chunk_id: u64,
    context_before: usize,
    context_after: usize,
) {
    let line_text = lines.get(line_idx).copied().unwrap_or("").to_string();
    let before_start = line_idx.saturating_sub(context_before);
    let after_end = (line_idx + 1 + context_after).min(lines.len());
    let context_before_v: Vec<String> = lines[before_start..line_idx]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let context_after_v: Vec<String> = lines[line_idx + 1..after_end]
        .iter()
        .map(|s| s.to_string())
        .collect();
    out.push(GrepMatch {
        file_path: file_path.to_string(),
        // start_line is 1-based; lines are 0-indexed within the chunk.
        line: chunk_start_line + line_idx as u32,
        column: col as u32 + 1,
        line_text,
        context_before: context_before_v,
        context_after: context_after_v,
        chunk_id,
    });
}

fn line_col_from_byte_offset(text: &str, offset: usize) -> (usize, usize) {
    let mut line_idx = 0usize;
    let mut last_line_start = 0usize;
    for (i, b) in text.bytes().enumerate() {
        if i >= offset {
            break;
        }
        if b == b'\n' {
            line_idx += 1;
            last_line_start = i + 1;
        }
    }
    (line_idx, offset.saturating_sub(last_line_start))
}

// ── Public helper (used by handler for result enrichment) ─────────────────────

/// Convenience: where the project lives on disk. Allows the handler
/// to resolve absolute paths for the freshness pass and context-line
/// readthrough.
pub fn project_root_from_directory(directory: &str) -> PathBuf {
    PathBuf::from(directory)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_scan_zero_files_warns_unconfirmed_coverage() {
        // Dead full-scan tier (scanned nothing) → loud, actionable warning.
        let w = full_scan_coverage_warning(GrepTier::FullScan, 0);
        assert!(w.is_some());
        assert!(w.unwrap().contains("UNCONFIRMED"));
        // Full-scan that DID scan files → no warning.
        assert!(full_scan_coverage_warning(GrepTier::FullScan, 12).is_none());
        // Other tiers never warn (they use the term index).
        assert!(full_scan_coverage_warning(GrepTier::TermIndex, 0).is_none());
        assert!(full_scan_coverage_warning(GrepTier::TermNarrowed, 0).is_none());
    }

    #[test]
    fn smart_case_sensitive_when_pattern_has_uppercase() {
        assert!(resolve_case_sensitive("SubmitChanges", None));
        assert!(resolve_case_sensitive("fooBAR", None));
        assert!(!resolve_case_sensitive("submit_changes", None));
        assert!(!resolve_case_sensitive("", None));
    }

    #[test]
    fn smart_case_honours_explicit_flag() {
        assert!(resolve_case_sensitive("submit", Some(true)));
        assert!(!resolve_case_sensitive("SUBMIT", Some(false)));
    }

    #[test]
    fn tier_selection_routes_short_literals_to_full_scan() {
        let mut q = GrepQuery {
            project_id: "p".into(),
            namespace: "code".into(),
            generation: 0,
            pattern: "ab".into(),
            regex: false,
            case_sensitive: None,
            multiline: false,
            path_prefix: None,
            language: None,
            context_before: 0,
            context_after: 0,
            max_results: 100,
            freshness: FreshnessMode::Off,
        };
        assert_eq!(pick_tier(&q), GrepTier::FullScan);
        q.pattern = "abc".into();
        assert_eq!(pick_tier(&q), GrepTier::TermIndex);
    }

    #[test]
    fn tier_selection_routes_regex_without_literal_anchor_to_full_scan() {
        let q = GrepQuery {
            project_id: "p".into(),
            namespace: "code".into(),
            generation: 0,
            pattern: "a.b".into(),
            regex: true,
            case_sensitive: None,
            multiline: false,
            path_prefix: None,
            language: None,
            context_before: 0,
            context_after: 0,
            max_results: 100,
            freshness: FreshnessMode::Off,
        };
        assert_eq!(pick_tier(&q), GrepTier::FullScan);
    }

    #[test]
    fn tier_selection_routes_anchored_regex_to_narrowed() {
        let q = GrepQuery {
            project_id: "p".into(),
            namespace: "code".into(),
            generation: 0,
            // `SubmitChanges` is a >=3-char literal run inside a regex.
            pattern: "SubmitChanges\\(.*\\)".into(),
            regex: true,
            case_sensitive: None,
            multiline: false,
            path_prefix: None,
            language: None,
            context_before: 0,
            context_after: 0,
            max_results: 100,
            freshness: FreshnessMode::Off,
        };
        assert_eq!(pick_tier(&q), GrepTier::TermNarrowed);
    }

    #[test]
    fn regex_literal_anchor_detection() {
        assert!(regex_has_literal_anchor("abc"));
        assert!(regex_has_literal_anchor("foo.bar"));
        assert!(!regex_has_literal_anchor("a.b"));
        assert!(!regex_has_literal_anchor(".*"));
        assert!(!regex_has_literal_anchor("a|b|c"));
        // Counted quantifiers now route conservatively through Tier 2.
        assert!(!regex_has_literal_anchor("x{1,2}abc"));
    }

    #[test]
    fn literal_anchor_extracts_longest_run() {
        assert_eq!(
            extract_literal_anchor("SubmitChanges").as_deref(),
            Some("SubmitChanges")
        );
        // `foo` and `bar` tie at 3 chars — extractor keeps the first.
        // Both are valid anchors from a correctness standpoint.
        let anchor = extract_literal_anchor("foo.*bar").unwrap();
        assert!(anchor == "foo" || anchor == "bar");
        // When lengths differ, the longer one wins.
        assert_eq!(
            extract_literal_anchor("foobar.*xyz").as_deref(),
            Some("foobar")
        );
    }

    #[test]
    fn literal_anchor_handles_escaped_metacharacters() {
        // `\(` is literal `(`, so the whole thing is a literal run.
        assert_eq!(
            extract_literal_anchor(r"SubmitChanges\(\)").as_deref(),
            Some("SubmitChanges()")
        );
        // Escaped dot stays in the literal.
        assert_eq!(
            extract_literal_anchor(r"foo\.bar").as_deref(),
            Some("foo.bar")
        );
    }

    #[test]
    fn literal_anchor_drops_optional_trailing_char() {
        // The `?` makes `o` optional — best literal is `fo`.
        assert_eq!(extract_literal_anchor("foo?bar").as_deref(), Some("bar"));
        // `a*b` — `a` is droppable, but `b` is standalone literal of
        // length 1 — too short, so no anchor.
        assert_eq!(extract_literal_anchor("a*b"), None);
    }

    #[test]
    fn literal_anchor_rejects_complex_constructs() {
        // Alternation — requires intersecting literals across branches.
        assert_eq!(extract_literal_anchor("foo|bar"), None);
        // Lookahead.
        assert_eq!(extract_literal_anchor("(?=abc)xyz"), None);
        // Counted quantifier.
        assert_eq!(extract_literal_anchor("abc{1,2}"), None);
    }

    #[test]
    fn literal_anchor_breaks_run_on_character_class_escapes() {
        // `\d` is a metaclass — should split the literal run.
        assert_eq!(extract_literal_anchor(r"foo\d+bar").as_deref(), Some("foo"));
        assert_eq!(
            extract_literal_anchor(r"foobar\d").as_deref(),
            Some("foobar")
        );
    }

    #[test]
    fn scan_chunk_finds_literal_with_line_and_column() {
        let content = "fn foo() {\n    let db = SubmitChanges();\n    bar();\n}\n";
        let mut matches = Vec::new();
        scan_chunk(
            content,
            100, // chunk_start_line (1-based, arbitrary)
            "SubmitChanges",
            true,
            false,
            false,
            "src/lib.rs",
            42,
            0,
            0,
            &mut matches,
            10,
        );
        assert_eq!(matches.len(), 1);
        let m = &matches[0];
        assert_eq!(m.line, 101); // chunk_start_line + 1 (second line of chunk)
        assert_eq!(m.column, 14); // byte offset of "SubmitChanges" on that line + 1
        assert_eq!(m.file_path, "src/lib.rs");
        assert_eq!(m.chunk_id, 42);
    }

    #[test]
    fn scan_chunk_supports_case_insensitive_literal() {
        let content = "SUBMITCHANGES();\n";
        let mut matches = Vec::new();
        scan_chunk(
            content,
            1,
            "submitchanges",
            false,
            false,
            false,
            "f.vb",
            1,
            0,
            0,
            &mut matches,
            10,
        );
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn scan_chunk_returns_context_lines() {
        let content = "a\nb\nc\nMATCH\ne\nf\n";
        let mut matches = Vec::new();
        scan_chunk(
            content,
            1,
            "MATCH",
            true,
            false,
            false,
            "f.rs",
            1,
            2,
            2,
            &mut matches,
            10,
        );
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].context_before, vec!["b", "c"]);
        assert_eq!(matches[0].context_after, vec!["e", "f"]);
    }

    #[test]
    fn scan_chunk_supports_regex_mode() {
        let content = "foo = 1\nfoo = 2\nfoo = three\n";
        let mut matches = Vec::new();
        scan_chunk(
            content,
            1,
            r"foo = \d+",
            true,
            true,
            false,
            "f.rs",
            1,
            0,
            0,
            &mut matches,
            10,
        );
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn scan_chunk_honours_max_results() {
        let content = "x\nx\nx\nx\nx\n";
        let mut matches = Vec::new();
        scan_chunk(
            content,
            1,
            "x",
            true,
            false,
            false,
            "f.rs",
            1,
            0,
            0,
            &mut matches,
            3,
        );
        assert_eq!(matches.len(), 3);
    }

    #[test]
    fn freshness_hash_confirms_touched_but_unchanged_file() {
        use std::io::Write as _;
        let dir = std::env::temp_dir().join(format!("engram_fresh_{}_unch", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let content = b"Class A\n  Sub Foo()\n  End Sub\nEnd Class\n";
        std::fs::File::create(dir.join("a.vb"))
            .unwrap()
            .write_all(content)
            .unwrap();
        // Indexed stat carries the REAL content hash but a deliberately stale
        // mtime — as if an editor/git touched the file after indexing without
        // changing a byte. This must NOT be reported stale.
        let stat = IndexedFileStat {
            rel_path: "a.vb".to_string(),
            size: content.len() as u64,
            mtime_secs: 1, // != disk mtime → forces the hash-confirm path
            file_hash: Some(blake3::hash(content).to_hex().to_string()),
        };
        let pid = format!("freshtest_unch_{}", std::process::id());
        let check = check_freshness(|| Ok(vec![stat]), &pid, "code", &dir).unwrap();
        assert!(
            check.stale_paths.is_empty(),
            "touched-but-unchanged file must NOT be stale: {:?}",
            check.stale_paths
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn freshness_flags_changed_content_and_skips_git() {
        use std::io::Write as _;
        let dir = std::env::temp_dir().join(format!("engram_fresh_{}_chg", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let content = b"real disk content\n";
        std::fs::File::create(dir.join("b.vb"))
            .unwrap()
            .write_all(content)
            .unwrap();
        // Indexed hash is for DIFFERENT content → genuinely changed → stale.
        let changed = IndexedFileStat {
            rel_path: "b.vb".to_string(),
            size: content.len() as u64,
            mtime_secs: 1,
            file_hash: Some(
                blake3::hash(b"old different content\n")
                    .to_hex()
                    .to_string(),
            ),
        };
        // A `.git/` file is never corpus and must be skipped outright.
        let git = IndexedFileStat {
            rel_path: ".git/index".to_string(),
            size: 999,
            mtime_secs: 1,
            file_hash: Some("whatever".to_string()),
        };
        let pid = format!("freshtest_chg_{}", std::process::id());
        let check = check_freshness(|| Ok(vec![changed, git]), &pid, "code", &dir).unwrap();
        assert_eq!(
            check.stale_paths,
            vec!["b.vb".to_string()],
            "changed content flagged; .git/ skipped"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
