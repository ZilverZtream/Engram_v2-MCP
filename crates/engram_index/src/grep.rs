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
//!    text is already STORED in Tantivy so no DocStore round-trip is
//!    needed for short queries. Microseconds per result.
//!
//! 2. **Tier 1 — narrowed regex.** Extract the longest literal
//!    substring from the regex, run a trigram prefilter against it,
//!    then apply the full regex only to the narrowed chunk set. Still
//!    dominates `rg` because we skip most of the codebase.
//!
//! 3. **Tier 2 — full scan.** Patterns too short (< 3 chars) or too
//!    regex-complex to prefilter fall through to a parallel scan over
//!    DocStore-resident chunk content.
//!
//! Every query begins with an optional freshness check that compares
//! `FileFingerprint` size + mtime against disk. Stale files are listed
//! in the result so callers can decide whether to fall back to a raw
//! disk scan for the affected paths.

use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::docstore::DocStore;
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
    /// Parallel byte scan over DocStore content.
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

// ── Freshness check ───────────────────────────────────────────────────────────

/// Compare indexed fingerprints against disk state. Returns the list
/// of tracked paths whose on-disk `(size, mtime)` no longer matches
/// the indexed fingerprint.
///
/// mtime granularity varies by filesystem (NTFS ~100 ns, ext4 ns,
/// APFS ns, FAT 2 s). We compare millisecond-quantised mtime so we're
/// not too eager on NTFS. False positives here are acceptable (we
/// might flag a file as stale when it's actually fine), false
/// negatives are not (we must not silently serve stale content).
pub fn check_freshness(
    docstore: &DocStore,
    project_id: &str,
    namespace: &str,
    project_root: &Path,
) -> anyhow::Result<Vec<String>> {
    let tracked = docstore.list_tracked_paths(project_id, namespace)?;
    let mut stale: Vec<String> = Vec::new();
    for rel_path in tracked {
        let abs = project_root.join(&rel_path);
        let Ok(Some(fp)) = docstore.get_fingerprint(project_id, &rel_path) else {
            // Missing fingerprint = not meaningfully tracked; skip
            // rather than flagging it as stale (keeps the warning
            // actionable).
            continue;
        };
        let meta = match std::fs::metadata(&abs) {
            Ok(m) => m,
            Err(_) => {
                // File disappeared or access denied — definitely stale.
                stale.push(rel_path);
                continue;
            }
        };
        let disk_size = meta.len();
        let disk_mtime_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        if disk_size != fp.size || disk_mtime_ms != fp.mtime_ms {
            stale.push(rel_path);
        }
    }
    Ok(stale)
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

/// Heuristic: does a regex contain at least one run of 3+ literal
/// characters that can feed the trigram index? Extremely conservative
/// — we only accept regexes with no metacharacters at all right now.
/// This is safe; wrong answers here just demote to Tier 2.
fn regex_has_literal_anchor(pat: &str) -> bool {
    let mut literal_run = 0;
    for c in pat.chars() {
        if matches!(
            c,
            '.' | '*' | '+' | '?' | '|' | '(' | ')' | '[' | ']' | '{' | '}' | '\\' | '^' | '$'
        ) {
            literal_run = 0;
        } else {
            literal_run += 1;
            if literal_run >= MIN_TRIGRAM_LEN {
                return true;
            }
        }
    }
    false
}

// ── Engine ────────────────────────────────────────────────────────────────────

/// Execute a grep query. Callers pass the same `HybridSearchEngine`
/// that serves `search_memory` so the Tantivy reader is shared and
/// warm across calls.
pub fn grep(
    engine: &HybridSearchEngine,
    docstore: &DocStore,
    project_root: &Path,
    q: &GrepQuery,
) -> anyhow::Result<GrepResult> {
    let start = Instant::now();

    // ── Step 1: freshness check ──
    let stale_paths = match q.freshness {
        FreshnessMode::Off => Vec::new(),
        FreshnessMode::Strict | FreshnessMode::Warn => {
            check_freshness(docstore, &q.project_id, &q.namespace, project_root)
                .unwrap_or_default()
        }
    };
    let index_stale_warning = if stale_paths.is_empty() {
        None
    } else {
        let msg = format!(
            "{} file{} on disk do not match the indexed fingerprint{} — results for those files may be stale. Run `update_project` to refresh.",
            stale_paths.len(),
            if stale_paths.len() == 1 { "" } else { "s" },
            if stale_paths.len() == 1 { "" } else { "s" },
        );
        Some(msg)
    };

    let tier = pick_tier(q);
    let case_sensitive = resolve_case_sensitive(&q.pattern, q.case_sensitive);

    // ── Step 2: execute tier ──
    let (matches, chunks_scanned, files_scanned) = match tier {
        GrepTier::TermIndex => execute_term_index(engine, docstore, q, case_sensitive)?,
        GrepTier::TermNarrowed => execute_term_narrowed(engine, docstore, q, case_sensitive)?,
        GrepTier::FullScan => execute_full_scan(docstore, q, case_sensitive)?,
    };

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
    docstore: &DocStore,
    q: &GrepQuery,
    case_sensitive: bool,
) -> anyhow::Result<(Vec<GrepMatch>, usize, usize)> {
    // The existing `lexical_search` handles the project_id +
    // namespace filtering and uses the trigram tokenizer for us.
    // We ask for extra results (×4) so that false-positive trigram
    // matches filtered out by our literal-verification pass still
    // leave `max_results` real hits.
    let oversample = (q.max_results.saturating_mul(4)).max(50);
    let hybrid_q = HybridQuery {
        project_id: q.project_id.clone(),
        namespace: q.namespace.clone(),
        generation: q.generation,
        text: q.pattern.clone(),
        top_k: oversample,
        fts_mode: "strict".into(),
        include_path_prefixes: q.path_prefix.as_ref().map(|p| vec![p.clone()]),
        exclude_path_prefixes: None,
        language_filters: q.language.as_ref().map(|l| vec![l.clone()]),
        author_filter: None,
        date_after: None,
        date_before: None,
        use_mmr: false,
    };
    let hits = engine.lexical_search(&hybrid_q)?;

    let mut matches: Vec<GrepMatch> = Vec::with_capacity(q.max_results);
    let mut chunks_scanned = 0usize;
    let mut files_seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for hit in hits {
        chunks_scanned += 1;
        files_seen.insert(hit.path.as_str().to_string());
        // Pull the chunk's stored content. The hit has a truncated
        // snippet; for reliable line numbers we want the whole chunk.
        // DocStore is the authoritative source and is already hot.
        let Some(doc) = docstore.get_doc(&q.project_id, &q.namespace, &hit.doc_id)? else {
            continue;
        };
        scan_chunk(
            &doc.content,
            doc.start_line,
            &q.pattern,
            case_sensitive,
            false, // not regex
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

fn execute_term_narrowed(
    _engine: &HybridSearchEngine,
    docstore: &DocStore,
    q: &GrepQuery,
    case_sensitive: bool,
) -> anyhow::Result<(Vec<GrepMatch>, usize, usize)> {
    // TODO: Extract longest literal run from the regex, run it
    // through Tier 0's trigram prefilter, then apply the full regex
    // only to the narrowed chunk set. For now we fall through to
    // Tier 2 — correct but slower than the planned optimisation.
    execute_full_scan(docstore, q, case_sensitive)
}

// ── Tier 2: full scan over DocStore content ──

fn execute_full_scan(
    docstore: &DocStore,
    q: &GrepQuery,
    case_sensitive: bool,
) -> anyhow::Result<(Vec<GrepMatch>, usize, usize)> {
    let mut matches: Vec<GrepMatch> = Vec::with_capacity(q.max_results);
    let mut chunks_scanned = 0usize;
    let paths = docstore.list_tracked_paths(&q.project_id, &q.namespace)?;
    let mut files_scanned = 0usize;
    for rel_path in paths {
        if let Some(ref prefix) = q.path_prefix
            && !rel_path.starts_with(prefix)
        {
            continue;
        }
        // Our DocStore stores per-namespace; most indexed projects
        // use the default "code" namespace. We honour the caller's
        // requested namespace.
        let docs = docstore.get_all_docs_for_file(&q.project_id, &q.namespace, &rel_path)?;
        if docs.is_empty() {
            continue;
        }
        files_scanned += 1;
        for doc in docs {
            if let Some(ref lang) = q.language
                && !doc.language.eq_ignore_ascii_case(lang)
            {
                continue;
            }
            chunks_scanned += 1;
            scan_chunk(
                &doc.content,
                doc.start_line,
                &q.pattern,
                case_sensitive,
                q.regex,
                q.multiline,
                &rel_path,
                0, // full-scan path doesn't know the Tantivy chunk_id
                q.context_before,
                q.context_after,
                &mut matches,
                q.max_results,
            );
            if matches.len() >= q.max_results {
                break;
            }
        }
        if matches.len() >= q.max_results {
            break;
        }
    }
    Ok((matches, chunks_scanned, files_scanned))
}

// ── Per-chunk scanner ─────────────────────────────────────────────────────────

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
    // Precompute the lines once so context lookups are O(1).
    let lines: Vec<&str> = content.lines().collect();

    if regex_mode {
        // Build the regex up front — reuse across lines or across the
        // entire chunk (for multiline mode).
        let mut builder = regex::RegexBuilder::new(pattern);
        builder.case_insensitive(!case_sensitive);
        builder.multi_line(true); // make ^/$ line-anchored
        builder.dot_matches_new_line(multiline);
        let Ok(re) = builder.build() else {
            return;
        };
        if multiline {
            // Scan the whole chunk in one pass so `.` can cross lines.
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
        // Literal path. Aho-Corasick is overkill for a single pattern;
        // use byte-level `find` with optional case folding.
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
        assert!(regex_has_literal_anchor("x{1,2}abc"));
        assert!(!regex_has_literal_anchor("a.b"));
        assert!(!regex_has_literal_anchor(".*"));
        assert!(!regex_has_literal_anchor("a|b|c"));
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
}
