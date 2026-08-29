//! Planning tools: the pre-implementation context an agent needs to turn a
//! one-line user story ("as an admin I want to set the minimum number of
//! photos") into a complete change in a legacy codebase.
//!
//! - `get_concept_footprint`: every touchpoint of a domain concept, grouped
//!   by role — the defense against "edited 2 of the 17 places the concept
//!   appears".
//! - `find_similar_changes`: historical commits most similar to a planned
//!   file set, and the recurring companion artifacts MISSING from it — the
//!   defense against "added the feature, forgot the admin page / menu entry".
//! - `find_implementation_pattern`: concrete exemplars of how this codebase
//!   already implements a pattern, with their data/state edges — agents
//!   imitate better than they invent.

use crate::handlers::validate_project_id;
use crate::models::{
    FindImplementationPatternRequest, FindSimilarChangesRequest, GetConceptFootprintRequest,
};
use crate::services::full_project_migration_service as full_mig;
use crate::services::pre_commit_review_service::{path_suffix_match, resolve_partner_to_current};
use crate::tools::Engram;
use engram_git::history::{GitWalker, MergeCommitPolicy};
use engram_graph::EdgeKind;
use engram_index::HybridQuery;
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;

// ── Pure helpers ─────────────────────────────────────────────────────────────

/// Lowercase concept stems used for substring matching: the term itself,
/// a naive singular (trailing 's' stripped), the clean `ies`->`y` / `es`
/// singular, and a compacted form without separators so "code category" also
/// matches "CodeCategory"/"code_category".
/// Row-4 audit: caps of the concept footprint, all REPORTED.
pub(crate) const ANCHOR_CAP: usize = 50;
pub(crate) const CONSUMER_CAP_PER_ANCHOR: usize = 200;
pub(crate) const LEXICAL_PAGE: usize = 2000;
/// Row-1 audit D10: gate types listed in the change-set brief's
/// "Permission gates" section; the cut is REPORTED (markdown + JSON).
pub(crate) const GATE_TYPE_CAP: usize = 10;
/// Gate DEFINITION files named in the same section; the cut is reported.
pub(crate) const GATE_DEF_FILE_CAP: usize = 2;

/// Row-4 audit A6/D6: the role a consumer plays towards an anchor table /
/// state key, derived from the edge kind + the source member name/path
/// (`src` is a node id `sym:function:<path>:<Class.Member>:<line>`).
/// Export means PRODUCING an export (export / excel / pdf / download, or an
/// .rdl / export path) — "report" is not an export word: live, the DAL reader
/// `GetCodeWithEstimateAndReportedQty` was mislabelled by it.
/// Bodies are NOT inspected: LINQ `InsertOnSubmit`/`DeleteOnSubmit` and SQL
/// verbs inside a neutrally named member stay `read` / `sql?` — the header
/// states that limit. Order: test path > export > delete > write > kind.
pub(crate) fn consumer_role(kind: &EdgeKind, src: &str) -> &'static str {
    let lower = src.to_ascii_lowercase();
    let segs: Vec<&str> = lower.split(':').collect();
    // `file:<path>` (report definitions, pages) vs `sym:<kind>:<path>:<member>:<line>`
    // — a file node has a path and no member; live, the `.rdl` reports are
    // file nodes and were classified as if their id had a third segment.
    let (path, member): (&str, &str) = match segs.as_slice() {
        ["file", p, ..] => (p, ""),
        [_, _, p, m, ..] => (p, m),
        [_, _, p] => (p, ""),
        _ => ("", lower.as_str()),
    };
    if crate::services::pre_commit_review_service::is_test_path(path) {
        return "test";
    }
    if path.ends_with(".rdl")
        || path.contains("export")
        || ["export", "excel", "pdf", "download"]
            .iter()
            .any(|w| member.contains(w))
    {
        return "export";
    }
    if member.contains("delete") || member.contains("remove") {
        return "delete";
    }
    if matches!(kind, EdgeKind::WritesState)
        || ["insert", "update", "save", "create", "write", "upsert"]
            .iter()
            .any(|w| member.contains(w))
    {
        return "write";
    }
    match kind {
        EdgeKind::SqlCalls => "sql?",
        _ => "read",
    }
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub(crate) struct FootprintCoverage {
    pub node_scan: String,
    pub anchors_matched: usize,
    pub anchors_used: usize,
    pub anchor_cap: usize,
    pub consumers: String,
    pub consumer_edges: usize,
    pub consumer_cap_per_anchor: usize,
    pub lexical: String,
    pub lexical_files: usize,
    pub lexical_hits: usize,
    pub lexical_page: usize,
    /// Literal (substring, case-insensitive) pass over the indexed chunk
    /// text — sees a stem INSIDE an identifier the tokenizer keeps whole.
    pub literal: String,
    pub literal_files: usize,
    pub literal_matches: usize,
    pub literal_cap: usize,
    pub failures: Vec<String>,
}

/// Matches requested = cap; a full page means more may exist.
pub(crate) const LITERAL_CAP: usize = 5000;
/// Ceiling for `max_per_group`: a discovery tool must let a caller list a
/// whole section (live: 137 text-only files were unreachable behind a
/// ceiling of 100 — row-4 A11). A cut is always printed as "… and N more".
pub(crate) const FOOTPRINT_GROUP_CEILING: usize = 500;

pub(crate) fn footprint_literal_status(matches: usize, cap: usize) -> &'static str {
    if matches >= cap {
        "truncated"
    } else {
        "complete"
    }
}

/// Files mentioned only in text: lexical + literal hits minus graph-bearing
/// files and vendor paths, sorted and deduplicated.
pub(crate) fn footprint_text_only_files(
    graph_files: &HashSet<&str>,
    lexical: &[String],
    literal: &[String],
) -> Vec<String> {
    let mut out: Vec<String> = lexical
        .iter()
        .chain(literal.iter())
        .filter(|f| !graph_files.contains(f.as_str()))
        .filter(|f| !engram_core::is_vendor_path(f))
        .cloned()
        .collect();
    out.sort();
    out.dedup();
    out
}

/// `complete` when the page was not filled past its size (we ask for
/// page + 1 hits), `truncated` otherwise.
pub(crate) fn footprint_lexical_status(hits: usize, page: usize) -> &'static str {
    if hits > page { "truncated" } else { "complete" }
}

/// Every matching table/state node is an anchor, bounded by ANCHOR_CAP
/// (reported when hit) — not the first five.
pub(crate) fn footprint_select_anchors(
    matched: &[(String, String)],
) -> (Vec<(String, String)>, bool) {
    let truncated = matched.len() > ANCHOR_CAP;
    (
        matched.iter().take(ANCHOR_CAP).cloned().collect(),
        truncated,
    )
}

pub(crate) fn render_footprint_coverage(c: &FootprintCoverage) -> String {
    let mut s = String::from("\n## Coverage\n");
    s.push_str(&format!("- node scan: {}\n", c.node_scan));
    s.push_str(&format!(
        "- anchors: {} matched, {} expanded (cap {})\n",
        c.anchors_matched, c.anchors_used, c.anchor_cap
    ));
    s.push_str(&format!(
        "- consumers: {} ({} edges; per-anchor cap {}, cap+1 fetch)\n",
        c.consumers, c.consumer_edges, c.consumer_cap_per_anchor
    ));
    s.push_str(&format!(
        "- lexical: {} ({} files from {} hits; page {}, cap+1 fetch)\n",
        c.lexical, c.lexical_files, c.lexical_hits, c.lexical_page
    ));
    s.push_str(&format!(
        "- literal: {} ({} files from {} matches; cap {})\n",
        c.literal, c.literal_files, c.literal_matches, c.literal_cap
    ));
    if !c.failures.is_empty() {
        s.push_str(&format!("- failures: {}\n", c.failures.join("; ")));
    }
    s
}

pub(crate) fn concept_stems(concept: &str) -> Vec<String> {
    let lower = concept.trim().to_lowercase();
    let mut stems = vec![lower.clone()];
    if let Some(singular) = lower.strip_suffix('s')
        && singular.len() >= 3
    {
        stems.push(singular.to_string());
    }
    // Clean English plural -> singular so a PLURAL concept matches SINGULAR code
    // identifiers: "roqentries" -> "roqentry" (matches RoqEntryService), which the
    // naive 's'-strip ("roqentrie") misses; "categories" -> "category". Guard the
    // base length so we never produce a tiny, over-matching stem.
    if let Some(base) = lower.strip_suffix("ies")
        && base.len() >= 3
    {
        let singular = format!("{base}y");
        if !stems.contains(&singular) {
            stems.push(singular);
        }
    } else if let Some(base) = lower.strip_suffix("es")
        && base.len() >= 4
        && !stems.iter().any(|s| s == base)
    {
        stems.push(base.to_string());
    }
    // Swedish morphology (row-4 audit D5): plural / definite suffixes so a
    // Swedish concept in either form matches the identifier in the other —
    // "kategorier" → "kategori", "projekten" → "projekt", "objektet" →
    // "objekt", "personalliggarna" → "personalliggar" (a prefix of the
    // singular). Base length ≥ 4 keeps tiny over-matching stems out
    // ("order" must not yield "ord").
    for suffix in ["erna", "arna", "orna", "na", "er", "ar", "or", "en", "et"] {
        if let Some(base) = lower.strip_suffix(suffix)
            && base.len() >= 4
            && !stems.iter().any(|s| s == base)
        {
            stems.push(base.to_string());
        }
    }
    if let Some(base) = lower.strip_suffix('n')
        && base.len() >= 4
        && base.ends_with(['a', 'e', 'i', 'o'])
        && !stems.iter().any(|s| s == base)
    {
        stems.push(base.to_string());
    }
    let compact: String = lower.chars().filter(|c| c.is_alphanumeric()).collect();
    if !stems.contains(&compact) && compact.len() >= 3 {
        stems.push(compact);
    }

    // Multi-word concepts: the whole phrase almost never appears verbatim in
    // identifiers ("user role and permission management" compacted to one
    // token matched NOTHING on a codebase full of role/permission code).
    // Add salient-word bigrams ("user role" → also "userrole") and long
    // single words. Connectives are dropped; short single words ("user",
    // "role") are NOT added alone — they over-match thousands of nodes.
    const CONNECTIVES: &[&str] = &[
        "and", "or", "of", "the", "a", "an", "to", "for", "in", "on", "with", "by", "from",
    ];
    let salient: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3 && !CONNECTIVES.contains(w))
        .collect();
    if salient.len() >= 2 {
        for pair in salient.windows(2) {
            let spaced = format!("{} {}", pair[0], pair[1]);
            let joined = format!("{}{}", pair[0], pair[1]);
            for s in [spaced, joined] {
                if !stems.contains(&s) {
                    stems.push(s);
                }
            }
        }
        for w in &salient {
            if w.len() >= 6 {
                let s = w.to_string();
                if !stems.contains(&s) {
                    stems.push(s);
                }
            }
        }
    }

    stems.retain(|s| !s.is_empty());
    stems
}

/// Split an identifier into lowercase word tokens on separators (`_`, `-`,
/// `.`, `/`, spaces) AND camelCase boundaries: `UserRoleProvider` →
/// ["user", "role", "provider"].
pub(crate) fn name_tokens(name: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in name.chars() {
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                tokens.push(cur.to_lowercase());
                cur.clear();
            }
            prev_lower = false;
            continue;
        }
        if c.is_uppercase() && prev_lower && !cur.is_empty() {
            tokens.push(cur.to_lowercase());
            cur.clear();
        }
        prev_lower = c.is_lowercase() || c.is_ascii_digit();
        cur.push(c);
    }
    if !cur.is_empty() {
        tokens.push(cur.to_lowercase());
    }
    tokens
}

/// Does `name` match any stem AT A TOKEN BOUNDARY?
///
/// The old raw-substring version made "order" match `reorder`,
/// `placeholder`, and `borderColor` — false touchpoints that propagated
/// into plan_user_story and get_change_set seeds. Now a stem matches when
/// (a) some identifier token equals or starts with it (`order` → `orders`,
/// NOT `reorder`), or (b) for multi-word stems, the concatenation of
/// CONSECUTIVE tokens starting at a token boundary begins with the stem's
/// compact form (`user role`/`userrole` → `UserRoleProvider`).
pub(crate) fn matches_concept(name: &str, stems: &[String]) -> bool {
    let tokens = name_tokens(name);
    if tokens.is_empty() {
        return false;
    }
    stems.iter().any(|s| {
        let s_compact: String = s.chars().filter(|c| c.is_alphanumeric()).collect();
        if s_compact.is_empty() {
            return false;
        }
        // Single-token check: equality or prefix at a token start.
        if tokens.iter().any(|t| t.starts_with(&s_compact)) {
            return true;
        }
        // Multi-token check: consecutive-token concatenation from each
        // boundary. Early-exits once the running concat outgrows the stem.
        for start in 0..tokens.len() {
            let mut concat = String::new();
            for t in &tokens[start..] {
                concat.push_str(t);
                if concat.len() >= s_compact.len() {
                    break;
                }
            }
            if concat.starts_with(&s_compact) {
                return true;
            }
        }
        false
    })
}

/// Token bag for change-shape similarity: full path, directory segments,
/// extension, and basename words (split on `_`, `-`, `.`, and case
/// boundaries). All lowercase.
pub(crate) fn path_token_bag(files: &[String]) -> HashSet<String> {
    let mut bag = HashSet::new();
    for f in files {
        let norm = f.replace('\\', "/").to_lowercase();
        bag.insert(norm.clone());
        let mut segments: Vec<&str> = norm.split('/').collect();
        let file_name = segments.pop().unwrap_or("");
        for seg in segments {
            if !seg.is_empty() {
                bag.insert(format!("dir:{seg}"));
            }
        }
        if let Some((stem, ext)) = file_name.rsplit_once('.') {
            bag.insert(format!("ext:{ext}"));
            // ASPX-family double extensions (aspx.cs / ascx.vb / designer.cs)
            if let Some((_, ext2)) = stem.rsplit_once('.') {
                bag.insert(format!("ext:{ext2}.{ext}"));
            }
            for word in stem.split(['_', '-', '.']) {
                if word.len() >= 3 {
                    bag.insert(format!("w:{word}"));
                }
            }
        }
    }
    bag
}

/// Jaccard similarity of two token bags.
pub(crate) fn bag_jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count();
    let union = a.len() + b.len() - inter;
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

/// Candidate transpile-partner paths for a (web-root-stripped, lowercased)
/// path: a TypeScript source's committed `.js` bundle, or a `.js` bundle's `.ts`
/// source. Generic transpile conventions only — a same-dir extension swap, or a
/// single output-dir swap (`ts/` <-> `~.js/` or `js/`) with an IDENTICAL
/// basename. The caller keeps only candidates that actually exist in the index,
/// so over-generation here is harmless. Empty for non-TS/JS paths.
pub(crate) fn transpile_pair_candidates(ps: &str) -> Vec<String> {
    let build = |src_exts: &[&str], out_exts: &[&str], dir_swaps: &[(&str, &str)]| -> Vec<String> {
        let stem = src_exts.iter().fold(ps.to_string(), |acc, e| {
            acc.strip_suffix(e).map(str::to_string).unwrap_or(acc)
        });
        if stem == ps {
            return Vec::new(); // ext didn't match (e.g. ".json" not ".js")
        }
        let mut bases = vec![stem.clone()];
        for (from, to) in dir_swaps {
            if stem.contains(from) {
                bases.push(stem.replacen(from, to, 1));
            }
        }
        bases
            .iter()
            .flat_map(|b| out_exts.iter().map(move |e| format!("{b}{e}")))
            .collect()
    };
    if ps.ends_with(".ts") || ps.ends_with(".tsx") {
        build(
            &[".tsx", ".ts"],
            &[".js", ".jsx"],
            &[("/ts/", "/~.js/"), ("/ts/", "/js/")],
        )
    } else if ps.ends_with(".js") || ps.ends_with(".jsx") {
        build(
            &[".jsx", ".js"],
            &[".ts", ".tsx"],
            &[("/~.js/", "/ts/"), ("/js/", "/ts/")],
        )
    } else {
        Vec::new()
    }
}

/// Interface <-> implementation pairing candidates for a (web-root-stripped,
/// lowercased) .NET class file: `Service.vb` pairs with `IService.vb` in the
/// same dir or an `interfaces/` subfolder, and vice versa. A signature change
/// to one side nearly always touches the other (live PR1913 recall miss:
/// `RoqEntryService.vb` ranked, `interfaces/IRoqEntryService.vb` didn't).
/// The caller keeps only candidates that EXIST in the index, so the
/// I-prefix over-generation ("invoice.vb" -> "nvoice.vb") is harmless.
/// Empty for markup code-behind/designer files (they pair via the page rule).
pub(crate) fn interface_pair_candidates(ps: &str) -> Vec<String> {
    let is_class_file = (ps.ends_with(".vb") || ps.ends_with(".cs") || ps.ends_with(".ml"))
        && !ps.ends_with(".designer.vb")
        && !ps.ends_with(".designer.cs")
        && !ps.ends_with(".aspx.vb")
        && !ps.ends_with(".aspx.cs")
        && !ps.ends_with(".ascx.vb")
        && !ps.ends_with(".ascx.cs");
    if !is_class_file {
        return Vec::new();
    }
    let Some(slash) = ps.rfind('/') else {
        return Vec::new();
    };
    let (dir, file) = ps.split_at(slash + 1);
    // Implementation -> its interface (same dir, or interfaces/ below).
    let mut out = vec![format!("{dir}i{file}"), format!("{dir}interfaces/i{file}")];
    // Interface -> its implementation (same dir, or the dir above an
    // interfaces/ folder).
    if let Some(stem) = file.strip_prefix('i').filter(|s| !s.is_empty()) {
        out.push(format!("{dir}{stem}"));
        if let Some(parent) = dir.strip_suffix("interfaces/") {
            out.push(format!("{parent}{stem}"));
        }
    }
    out
}

/// OpenAPI/Swagger contract documents in a (lowercased) file index. The
/// spec is an ASSERTED CONTRACT for the API layer — endpoint/DTO changes
/// ship a spec update (recurring recall miss: docs/openapi/*.yaml shipped
/// with every RoQ endpoint change but never ranked — no code edge reaches
/// a yaml). Vendor-filtered, sorted for determinism, capped: a
/// spec-per-service repo could hold dozens.
pub(crate) fn api_spec_docs(index: &[String]) -> Vec<String> {
    let mut specs: Vec<String> = index
        .iter()
        .filter(|f| {
            (f.contains("openapi") || f.contains("swagger"))
                && (f.ends_with(".yaml") || f.ends_with(".yml") || f.ends_with(".json"))
                && !engram_core::is_vendor_path(f)
        })
        .cloned()
        .collect();
    specs.sort();
    specs.truncate(3);
    specs
}

/// True when a (lowercased, web-root-stripped) candidate path is API-layer
/// code: a code file with a path segment that names an api surface
/// (`api`, `api-v2`, `api-json`, `apis`, …).
pub(crate) fn is_api_code_path(ps: &str) -> bool {
    (ps.ends_with(".vb")
        || ps.ends_with(".cs")
        || ps.ends_with(".ts")
        || ps.ends_with(".js")
        || ps.ends_with(".ml"))
        && ps.split('/').any(|seg| {
            seg.starts_with("api") && seg.len() <= 12 // segment NAMES an api surface, not e.g. "apiary-docs-archive"
        })
}

/// True when a function node name is a `Can<Something>` permission helper
/// (`CanUserUploadMarkerIcon`, `aspnetUsers.CanEditRoq`) — the naming
/// convention shared across C#/VB/TS codebases. Checks the LAST dotted
/// segment; requires an uppercase after "Can" so `Cancel`, `Canonical`
/// and `scan` don't match.
pub(crate) fn is_can_helper_name(name: &str) -> bool {
    let last = name.rsplit('.').next().unwrap_or(name);
    let mut chars = last.chars();
    matches!(
        (chars.next(), chars.next(), chars.next(), chars.next()),
        (Some('C'), Some('a'), Some('n'), Some(c)) if c.is_ascii_uppercase()
    )
}

/// "dir/.../*.ext" shape of a path, used to report recurring companion
/// patterns ("Admin/*.aspx") instead of raw historical file names.
pub(crate) fn dir_ext_shape(path: &str) -> Option<String> {
    let norm = path.replace('\\', "/");
    let (dir, file) = norm.rsplit_once('/').unwrap_or(("", norm.as_str()));
    let ext = file.split_once('.').map(|(_, e)| e)?;
    Some(if dir.is_empty() {
        format!("*.{ext}")
    } else {
        format!("{dir}/*.{ext}")
    })
}

/// Wall-clock budget for the co-change git walk.
///
/// A cold walk over `max_commits` commits costs one diff each; on a large
/// repo that is minutes, and the caller has no way to cancel it. Past the
/// budget the walk stops and the answer says its coverage is partial;
/// subsequent calls resume from the cache and finish the rest.
/// External audit 2026-08-29 row 9 / P0-3 latency: the co-change snapshot
/// (per-commit incremental walk, disk-persisted) used to be built by the
/// FIRST caller of find_similar_changes / get_change_set — 11.7 s live. It is
/// now built here and warmed at index / update time, so call time only reads.
/// Returns the snapshot plus this walk's commits, its walked count, the number
/// of fresh diffs and whether the time budget cut the walk short.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_co_change_snapshot(
    cache: &dashmap::DashMap<String, std::sync::Arc<crate::state::CoChangeSnapshot>>,
    cache_key: String,
    disk_path: &std::path::Path,
    repo_dir: &std::path::Path,
    max_commits: usize,
    budget: std::time::Duration,
    started: std::time::Instant,
) -> anyhow::Result<(
    std::sync::Arc<crate::state::CoChangeSnapshot>,
    Vec<crate::state::CoChangeCommit>,
    usize,
    usize,
    bool,
)> {
    let repo = GitWalker::open_repo(repo_dir)?;
    let repo = GitWalker::open_repo(&repo_dir)?;
    let head = repo
        .head()
        .ok()
        .and_then(|h| h.target())
        .map(|o| o.to_string())
        .unwrap_or_default();

    // Reuse is per COMMIT, not per HEAD. The old cache was keyed on
    // HEAD, so a single new commit invalidated the whole snapshot and
    // the next call re-diffed every one of max_commits commits — the
    // reason this tool could sit for minutes on an active repo while
    // detect_incomplete_changes answered the neighbouring question
    // instantly from precomputed edges.
    let cached: Option<std::sync::Arc<crate::state::CoChangeSnapshot>> = match cache.get(&cache_key)
    {
        Some(s) => Some(s.clone()),
        None => std::fs::read(&disk_path)
            .ok()
            .and_then(|bytes| bincode::deserialize(&bytes).ok())
            .map(std::sync::Arc::new),
    };

    let mut known: HashMap<String, crate::state::CoChangeCommit> = HashMap::new();
    let mut already_diffed: HashSet<String> = HashSet::new();
    if let Some(prev) = &cached {
        already_diffed.extend(prev.walked_oids.iter().cloned());
        for c in &prev.commits {
            known.insert(c.oid.clone(), c.clone());
        }
        // Snapshots written before walked_oids existed carry only the
        // surviving commits; treat those as the diffed set so they are
        // still reusable.
        if prev.walked_oids.is_empty() {
            already_diffed.extend(known.keys().cloned());
        }
    }

    // Walking oids is cheap; only the per-commit diff is not.
    let cancel = tokio_util::sync::CancellationToken::new();
    let oids = GitWalker::walk_older_commits(
        &repo,
        None,
        max_commits,
        MergeCommitPolicy::FirstParentOnly,
        &cancel,
    )?;

    let mut commits = Vec::with_capacity(oids.len());
    let mut walked_oids = Vec::with_capacity(oids.len());
    let mut partial = false;
    let mut fresh_diffs = 0usize;
    for oid in &oids {
        let key = oid.to_string();
        if already_diffed.contains(&key) {
            walked_oids.push(key.clone());
            if let Some(c) = known.get(&key) {
                commits.push(c.clone());
            }
            continue;
        }

        // Budget guard. A cold walk on a big repo must degrade to
        // partial coverage, never to a multi-minute hang that the
        // caller cannot cancel.
        if started.elapsed() >= budget {
            partial = true;
            break;
        }

        let Ok(changes) = GitWalker::files_changed_in_commit(&repo, *oid) else {
            continue;
        };
        fresh_diffs += 1;
        walked_oids.push(key.clone());
        // Bulk commits (vendoring, formatting) are shape noise. They
        // still go in walked_oids so they are never re-diffed.
        if changes.len() > 80 || changes.is_empty() {
            continue;
        }
        let files: Vec<String> = changes
            .iter()
            .map(|c| c.path().as_str().replace('\\', "/"))
            .collect();
        let summary = repo
            .find_commit(*oid)
            .ok()
            .and_then(|c| c.summary().map(|s| s.to_string()))
            .unwrap_or_default();
        let commit = crate::state::CoChangeCommit {
            oid: key.clone(),
            summary,
            files,
        };
        commits.push(commit.clone());
        known.insert(key, commit);
    }

    // Anything the previous snapshot knew about but this walk did not
    // reach (deeper history from a larger earlier max_commits) stays
    // reusable for a later, deeper call.
    let reached: HashSet<&String> = walked_oids.iter().collect();
    let mut carried_oids: Vec<String> = already_diffed
        .iter()
        .filter(|o| !reached.contains(*o))
        .cloned()
        .collect();
    carried_oids.sort();
    let mut all_oids = walked_oids.clone();
    all_oids.extend(carried_oids.iter().cloned());
    let mut all_commits = commits.clone();
    for o in &carried_oids {
        if let Some(c) = known.get(o) {
            all_commits.push(c.clone());
        }
    }

    let snap = std::sync::Arc::new(crate::state::CoChangeSnapshot {
        head,
        walked: max_commits.max(cached.as_ref().map(|c| c.walked).unwrap_or(0)),
        commits: all_commits,
        walked_oids: all_oids,
        partial,
    });
    cache.insert(cache_key, snap.clone());
    // Best-effort disk persist for the next cold start.
    if let Ok(bytes) = bincode::serialize(snap.as_ref())
        && let Some(parent) = disk_path.parent()
    {
        let _ = std::fs::create_dir_all(parent);
        let _ = std::fs::write(&disk_path, bytes);
    }
    Ok((snap, commits, walked_oids.len(), fresh_diffs, partial))
}

/// Warm the co-change snapshot for a project (index / update completion).
pub(crate) fn warm_co_change_snapshot_blocking(
    state: &crate::state::AppState,
    project_id: &str,
) -> anyhow::Result<()> {
    let rec = state
        .registry
        .get_project(project_id)?
        .ok_or_else(|| anyhow::anyhow!("project {project_id} not registered"))?;
    let disk_path = state
        .cfg
        .data_dir
        .join("co_change")
        .join(format!("{project_id}.bin"));
    let _ = build_co_change_snapshot(
        &state.co_change_cache,
        project_id.to_string(),
        &disk_path,
        std::path::Path::new(&rec.directory),
        500,
        co_change_budget(),
        std::time::Instant::now(),
    )?;
    Ok(())
}

fn co_change_budget() -> std::time::Duration {
    let secs = std::env::var("ENGRAM_CO_CHANGE_BUDGET_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(20);
    std::time::Duration::from_secs(secs)
}

impl Engram {
    // ── get_concept_footprint ────────────────────────────────────────────────

    pub async fn handle_get_concept_footprint(
        &self,
        req: GetConceptFootprintRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        if req.concept.trim().len() < 2 {
            return Err(McpError::invalid_params(
                "concept must be at least 2 characters",
                None,
            ));
        }
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;
        let cap = req.max_per_group.clamp(1, FOOTPRINT_GROUP_CEILING);
        let stems = concept_stems(&req.concept);

        // One graph scan + bounded consumer expansion, all in one blocking hop.
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let stems_b = stems.clone();
        type Entry = (String, String, String, u32); // name, node_id, file, line
        let (groups, consumers, scan_truncated, mut cov) = tokio::task::spawn_blocking(move || {
            let mut cov = FootprintCoverage {
                anchor_cap: ANCHOR_CAP,
                consumer_cap_per_anchor: CONSUMER_CAP_PER_ANCHOR,
                lexical_page: LEXICAL_PAGE,
                literal_cap: LITERAL_CAP,
                ..Default::default()
            };
            let nodes =
                match graph.query_nodes(&pid, None, None, None, crate::handlers::NODE_SCAN_LIMIT) {
                    Ok(n) => {
                        cov.node_scan = "complete".into();
                        n
                    }
                    Err(e) => {
                        cov.node_scan = "failed".into();
                        cov.failures.push(format!("node scan failed: {e}"));
                        Vec::new()
                    }
                };
            let scan_truncated = nodes.len() >= crate::handlers::NODE_SCAN_LIMIT;
            if scan_truncated {
                cov.node_scan = "truncated".into();
            }

            let mut groups: BTreeMap<&'static str, Vec<Entry>> = BTreeMap::new();
            let mut anchors: Vec<(String, String)> = Vec::new(); // (node_id, name)
            for n in &nodes {
                if !matches_concept(&n.name, &stems_b) {
                    continue;
                }
                let group = match n.node_type.as_str() {
                    "db_table" | "db_column" => "data",
                    "stored_proc" | "stored_procedure" | "inline_sql" => "sql",
                    "global_state" => "state",
                    "page" | "control" | "ui_container" | "control_layout" => "ui",
                    "function" | "class" | "interface" => "logic",
                    "web_service" | "http_handler" | "wcf_service" | "route_handler" => "endpoints",
                    "file" => "files",
                    _ => continue,
                };
                if matches!(n.node_type.as_str(), "db_table" | "global_state") {
                    anchors.push((n.node_id.clone(), n.name.clone()));
                }
                groups.entry(group).or_default().push((
                    n.name.clone(),
                    n.node_id.clone(),
                    n.file_path.as_str().to_string(),
                    n.start_line,
                ));
            }
            for list in groups.values_mut() {
                list.sort();
                list.dedup();
            }

            // Consumers of the anchor tables / state keys: who reads/writes.
            cov.anchors_matched = anchors.len();
            let (anchors, anchors_truncated) = footprint_select_anchors(&anchors);
            cov.anchors_used = anchors.len();
            if anchors_truncated {
                cov.failures.push(format!(
                    "anchors capped at {ANCHOR_CAP} of {}",
                    cov.anchors_matched
                ));
            }
            let mut consumers: Vec<(String, String, String)> = Vec::new(); // anchor, role:kind, src
            cov.consumers = "complete".into();
            for (anchor_id, anchor_name) in &anchors {
                match graph.find_incoming_edges_with_kind(
                    &pid,
                    None,
                    anchor_id,
                    CONSUMER_CAP_PER_ANCHOR + 1,
                ) {
                    Ok(incoming) => {
                        if incoming.len() > CONSUMER_CAP_PER_ANCHOR {
                            cov.consumers = "truncated".into();
                        }
                        for (src, kind, _w) in incoming.into_iter().take(CONSUMER_CAP_PER_ANCHOR) {
                            cov.consumer_edges += 1;
                            if matches!(
                                kind,
                                EdgeKind::QueriesTable
                                    | EdgeKind::ReadsColumn
                                    | EdgeKind::ReadsState
                                    | EdgeKind::WritesState
                                    | EdgeKind::SqlCalls
                                    | EdgeKind::DataBinding
                            ) {
                                consumers.push((
                                    anchor_name.clone(),
                                    format!("{}:{}", consumer_role(&kind, &src), kind.as_str()),
                                    src,
                                ));
                            }
                        }
                    }
                    Err(e) => {
                        cov.consumers = "failed".into();
                        cov.failures
                            .push(format!("consumer lookup failed for {anchor_name}: {e}"));
                    }
                }
            }
            consumers.sort();
            consumers.dedup();
            (groups, consumers, scan_truncated, cov)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // Lexical layer: files mentioning the concept that no graph group has.
        let engine = ps.search.clone();
        let q = HybridQuery {
            project_id: req.project_id.clone(),
            namespace: "memory".into(),
            generation: gen_,
            text: req.concept.clone(),
            top_k: LEXICAL_PAGE + 1,
            fts_mode: "loose".into(),
            include_path_prefixes: None,
            exclude_path_prefixes: None,
            language_filters: None,
            author_filter: None,
            date_after: None,
            date_before: None,
            use_mmr: false,
        };
        let (lexical_files, lexical_hits, lexical_err): (Vec<String>, usize, Option<String>) =
            tokio::task::spawn_blocking(move || match engine.lexical_search(&q) {
                Ok(hits) => {
                    let n = hits.len();
                    let mut files: Vec<String> =
                        hits.iter().map(|h| h.path.as_str().to_string()).collect();
                    files.sort();
                    files.dedup();
                    (files, n, None)
                }
                Err(e) => (Vec::new(), 0, Some(e.to_string())),
            })
            .await
            .unwrap_or_else(|e| (Vec::new(), 0, Some(format!("lexical task failed: {e}"))));
        cov.lexical_hits = lexical_hits;
        cov.lexical_files = lexical_files.len();
        cov.lexical = match lexical_err {
            Some(e) => {
                cov.failures.push(format!("lexical search failed: {e}"));
                "failed".into()
            }
            None => footprint_lexical_status(lexical_hits, LEXICAL_PAGE).into(),
        };

        // Literal pass (row-4 audit A2): substring, case-insensitive, over the
        // indexed chunk text — the tokenized index cannot see the stem inside
        // `rk_redovisningskategorier`; this can.
        let rec_dir = self
            .ensure_project_record(&req.project_id)
            .await
            .map(|r| r.directory)
            .unwrap_or_default();
        let (literal_files, literal_matches, literal_err): (Vec<String>, usize, Option<String>) = {
            let engine = ps.search.clone();
            let graph = self.state.graph.clone();
            let pid = req.project_id.clone();
            let gq = engram_index::grep::GrepQuery {
                project_id: req.project_id.clone(),
                namespace: "memory".into(),
                generation: gen_,
                pattern: req.concept.trim().to_string(),
                regex: false,
                case_sensitive: Some(false),
                multiline: false,
                path_prefix: None,
                language: None,
                context_before: 0,
                context_after: 0,
                max_results: LITERAL_CAP,
                freshness: engram_index::grep::FreshnessMode::Off,
            };
            let root = std::path::PathBuf::from(rec_dir);
            tokio::task::spawn_blocking(move || {
                match engram_index::grep::grep(&engine, &root, &gq, || {
                    crate::handlers::grep_tools::indexed_file_stats(&graph, &pid)
                }) {
                    Ok(r) => {
                        let n = r.matches.len();
                        let mut files: Vec<String> = r
                            .matches
                            .iter()
                            .map(|m| m.file_path.replace('\\', "/"))
                            .collect();
                        files.sort();
                        files.dedup();
                        (files, n, None)
                    }
                    Err(e) => (Vec::new(), 0, Some(e.to_string())),
                }
            })
            .await
            .unwrap_or_else(|e| (Vec::new(), 0, Some(format!("literal task failed: {e}"))))
        };
        cov.literal_matches = literal_matches;
        cov.literal_files = literal_files.len();
        cov.literal = match literal_err {
            Some(e) => {
                cov.failures.push(format!("literal pass failed: {e}"));
                "failed".into()
            }
            None => footprint_literal_status(literal_matches, LITERAL_CAP).into(),
        };

        let grouped_files: HashSet<&str> = groups
            .values()
            .flatten()
            .map(|(_, _, file, _)| file.as_str())
            .collect();
        // ENG-2026-CFP-NOISE: drop vendored/minified/bundled artifacts (bower,
        // node_modules, *.min.js, versioned jquery, …). The pilot eval found the
        // concept-footprint lexical layer was the dominant noise source handed to
        // the model (precision ~5%); these generated files are never the change
        // target and crowd out the real files.
        let text_only: Vec<String> =
            footprint_text_only_files(&grouped_files, &lexical_files, &literal_files);
        let lexical_only: Vec<&String> = text_only.iter().collect();

        let total: usize = groups.values().map(Vec::len).sum();
        if total == 0 && lexical_only.is_empty() {
            let mut out = format!(
                "No touchpoints found for concept '{}' (stems tried: {}).\n\
                 hints: try a shorter stem (e.g. \"photo\" not \"photographs\"); \
                 search_memory with fts_mode=\"loose\" to discover the codebase's \
                 actual vocabulary for this concept. If the concept's files are new since \
                 the last index they are invisible here — grep_project / read the working \
                 tree before concluding it's absent.",
                req.concept,
                stems.join(", ")
            );
            out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
            return Ok(CallToolResult::success(vec![Content::text(out)]));
        }

        let mut out = format!(
            "# Concept footprint: '{}'\n\nstems matched: {} | graph touchpoints: {}\n",
            req.concept,
            stems.join(", "),
            total
        );
        if scan_truncated {
            out.push_str(
                "⚠ node scan hit the node-scan cap — touchpoints may be incomplete on this \
                 graph; narrow the concept or rely on the lexical section below.\n",
            );
        }
        let titles = [
            ("data", "## Data (tables / columns)"),
            ("sql", "## SQL (stored procs / inline)"),
            (
                "state",
                "## Shared state (Session / ViewState / Cache keys)",
            ),
            ("ui", "## UI (pages / controls)"),
            ("logic", "## Logic (functions / classes)"),
            ("endpoints", "## Endpoints (services / routes)"),
            ("files", "## Files"),
        ];
        for (key, title) in titles {
            let Some(list) = groups.get(key) else {
                continue;
            };
            out.push_str(&format!("\n{title} — {}\n", list.len()));
            for (name, node_id, file, line) in list.iter().take(cap) {
                if file.is_empty() {
                    out.push_str(&format!("- {name} — node_id={node_id}\n"));
                } else {
                    out.push_str(&format!("- {name} — node_id={node_id} ({file}:{line})\n"));
                }
            }
            if list.len() > cap {
                out.push_str(&format!("  ... and {} more\n", list.len() - cap));
            }
        }

        if !consumers.is_empty() {
            let role_count = |role: &str| {
                consumers
                    .iter()
                    .filter(|(_, rk, _)| rk.split(':').next() == Some(role))
                    .count()
            };
            out.push_str(&format!(
                "\n## Consumers of core anchors — {} (write {}, read {}, delete {}, export {}, test {}, sql? {}; role from edge kind + source member name/path — bodies not inspected, so a `read` may still write via LINQ/SQL)\n",
                consumers.len(),
                role_count("write"),
                role_count("read"),
                role_count("delete"),
                role_count("export"),
                role_count("test"),
                role_count("sql?"),
            ));
            for (anchor, kind, src) in consumers.iter().take(cap * 2) {
                out.push_str(&format!("- [{kind}] {src} -> {anchor}\n"));
            }
            if consumers.len() > cap * 2 {
                out.push_str(&format!("  ... and {} more\n", consumers.len() - cap * 2));
            }
        }

        if !lexical_only.is_empty() {
            out.push_str(&format!(
                "\n## Mentioned only in text — {} file(s) the graph has no concept edge for (verify manually; lexical {}, literal {})\n",
                lexical_only.len(),
                cov.lexical,
                cov.literal
            ));
            for f in lexical_only.iter().take(cap) {
                out.push_str(&format!("- {f}\n"));
            }
            if lexical_only.len() > cap {
                out.push_str(&format!("  ... and {} more\n", lexical_only.len() - cap));
            }
        }

        out.push_str(&render_footprint_coverage(&cov));
        out.push_str(NEXT_STEPS_CONCEPT_FOOTPRINT);
        out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    // ── find_similar_changes ─────────────────────────────────────────────────

    pub async fn handle_find_similar_changes(
        &self,
        req: FindSimilarChangesRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        if req.files.is_empty() {
            return Err(McpError::invalid_params(
                "files must contain at least one planned/changed file path",
                None,
            ));
        }
        let rec = self.ensure_project_record(&req.project_id).await?;
        let gen_ = self
            .get_active_generation(&req.project_id)
            .await
            .unwrap_or(1);
        let max_commits = req.sanitized_max_commits();
        let top = req.sanitized_top();
        let input_files: Vec<String> = req.files.iter().map(|f| f.replace('\\', "/")).collect();
        let repo_dir = PathBuf::from(&rec.directory);

        let input_bag = path_token_bag(&input_files);
        let input_set: HashSet<String> = input_files.iter().map(|f| f.to_lowercase()).collect();

        let started = std::time::Instant::now();
        let cache = self.state.co_change_cache.clone();
        let cache_key = req.project_id.clone();
        // Disk-persisted snapshot: the in-memory cache dies with the daemon,
        // making every cold start pay the walk again. History is immutable,
        // so a bincode dump is exact.
        let disk_path = self
            .state
            .cfg
            .data_dir
            .join("co_change")
            .join(format!("{}.bin", req.project_id));
        let budget = co_change_budget();
        let result = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let (_snap, commits, scanned_len, fresh_diffs, partial) = build_co_change_snapshot(
                &cache,
                cache_key,
                &disk_path,
                &repo_dir,
                max_commits,
                budget,
                started,
            )?;
            let walked_oids_len = scanned_len;
            if fresh_diffs > 0 {
                tracing::debug!(
                    fresh_diffs,
                    reused = walked_oids_len.saturating_sub(fresh_diffs),
                    partial,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "find_similar_changes: co-change walk"
                );
            }

            let scanned = walked_oids_len;
            let mut scored: Vec<(f64, String, String, Vec<String>)> = Vec::new();
            for c in &commits {
                let score = bag_jaccard(&input_bag, &path_token_bag(&c.files));
                if score <= 0.0 {
                    continue;
                }
                scored.push((score, c.oid.clone(), c.summary.clone(), c.files.clone()));
            }
            scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            scored.truncate(top);
            Ok((scanned, scored, partial))
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| {
            McpError::internal_error(
                format!(
                    "find_similar_changes: cannot walk git history at '{}': {e}. \
                     hint: the project directory must be a git repository.",
                    rec.directory
                ),
                None,
            )
        })?;
        let (scanned, scored, partial) = result;

        if scored.is_empty() {
            let mut out = format!(
                "No similar historical changes found ({scanned} commits scanned).\n\
                 hints: pass more representative file paths; raise max_commits; \
                 this also happens when the planned files share no naming/directory \
                 conventions with past work — worth a closer look in itself."
            );
            if partial {
                out.push_str(
                    "\nNOTE: the history walk hit its time budget, so this is PARTIAL \
                     coverage, not a proven absence. Call again - the walk resumes \
                     from where it stopped.\n",
                );
            }
            out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
            return Ok(CallToolResult::success(vec![Content::text(out)]));
        }

        // Companion analysis: what do the similar changes touch that the plan
        // doesn't? Exact recurring files first (menu/config/registration files
        // recur verbatim), then recurring dir/*.ext shapes.
        let threshold = scored.len().div_ceil(2);
        let mut exact_counts: HashMap<&str, usize> = HashMap::new();
        let mut shape_counts: HashMap<String, usize> = HashMap::new();
        for (_, _, _, files) in &scored {
            let mut seen_shapes = HashSet::new();
            for f in files {
                if !input_set.contains(&f.to_lowercase()) {
                    *exact_counts.entry(f.as_str()).or_default() += 1;
                    if let Some(shape) = dir_ext_shape(f)
                        && seen_shapes.insert(shape.clone())
                    {
                        *shape_counts.entry(shape).or_default() += 1;
                    }
                }
            }
        }
        let input_shapes: HashSet<String> = input_files
            .iter()
            .filter_map(|f| dir_ext_shape(f))
            .collect();
        let mut recurring_exact: Vec<(&str, usize)> = exact_counts
            .into_iter()
            .filter(|(_, c)| *c >= threshold)
            .collect();
        recurring_exact.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        let mut recurring_shapes: Vec<(String, usize)> = shape_counts
            .into_iter()
            .filter(|(s, c)| *c >= threshold && !input_shapes.contains(s))
            .collect();
        recurring_shapes.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

        let mut out = format!(
            "# Similar historical changes ({} of {scanned} commits scanned, {:.1}s)\n{}\nYour set: {}\n",
            scored.len(),
            started.elapsed().as_secs_f32(),
            if partial {
                "\nPARTIAL: the walk hit its time budget before covering the requested \
                 depth. Call again to extend it - already-diffed commits are reused.\n"
            } else {
                ""
            },
            input_files.join(", ")
        );
        for (i, (score, hash, summary, files)) in scored.iter().enumerate() {
            out.push_str(&format!(
                "\n## #{} {} (similarity {:.2}) — {}\n",
                i + 1,
                &hash[..10.min(hash.len())],
                score,
                summary
            ));
            for f in files.iter().take(25) {
                let marker = if input_set.contains(&f.to_lowercase()) {
                    " ← in your set"
                } else {
                    ""
                };
                out.push_str(&format!("- {f}{marker}\n"));
            }
            if files.len() > 25 {
                out.push_str(&format!("  ... and {} more\n", files.len() - 25));
            }
        }

        if recurring_exact.is_empty() && recurring_shapes.is_empty() {
            out.push_str("\n## Companion check\nNo recurring companion artifacts are missing from your set.\n");
        } else {
            out.push_str(&format!(
                "\n## Changes like this also touched — MISSING from your set (≥{threshold} of {} commits)\n",
                scored.len()
            ));
            for (f, c) in recurring_exact.iter().take(15) {
                out.push_str(&format!("- {f} ({c}x) ← exact file, strong signal\n"));
            }
            for (s, c) in recurring_shapes.iter().take(15) {
                out.push_str(&format!("- {s} ({c}x)\n"));
            }
            out.push_str(
                "\nReview each before committing: these are the artifacts a reviewer \
                 will notice are absent (admin pages, menu/sitemap entries, \
                 registrations, migrations).\n",
            );
        }
        out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    // ── find_implementation_pattern ──────────────────────────────────────────

    pub async fn handle_find_implementation_pattern(
        &self,
        req: FindImplementationPatternRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        if req.pattern_query.trim().is_empty() {
            return Err(McpError::invalid_params(
                "pattern_query must not be empty",
                None,
            ));
        }
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;
        let max_examples = req.max_examples.clamp(1, 10);
        let kind = infer_pattern_kind(&req.pattern_query);
        let mut coverage = PatternCoverage {
            lexical_hits: 0,
            lexical_cap: PATTERN_LEXICAL_CAP,
            lexical_status: "complete".into(),
            lexical_files: 0,
            kind_filter: kind.as_str().into(),
            kind_filter_applied: false,
            kind_matched_files: 0,
            candidates_considered: 0,
            candidates_cap: PATTERN_CANDIDATES,
            exemplar_cap: max_examples,
            handlers_cap: PATTERN_HANDLERS_CAP,
            controls_cap: PATTERN_CONTROLS_CAP,
            data_cap: PATTERN_DATA_CAP,
            chain_depth_cap: PATTERN_CHAIN_DEPTH,
            failures: Vec::new(),
        };

        // Lexical candidates: cap+1 so truncation is a fact, not a guess.
        let engine = ps.search.clone();
        let q = HybridQuery {
            project_id: req.project_id.clone(),
            namespace: "memory".into(),
            generation: gen_,
            text: req.pattern_query.clone(),
            top_k: PATTERN_LEXICAL_CAP + 1,
            fts_mode: "loose".into(),
            include_path_prefixes: None,
            exclude_path_prefixes: None,
            language_filters: None,
            author_filter: None,
            date_after: None,
            date_before: None,
            use_mmr: false,
        };
        let hits = tokio::task::spawn_blocking(move || engine.lexical_search(&q))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(format!("lexical search failed: {e}"), None))?;
        coverage.lexical_hits = hits.len().min(PATTERN_LEXICAL_CAP);
        if hits.len() > PATTERN_LEXICAL_CAP {
            coverage.lexical_status = "truncated".into();
        }
        if hits.is_empty() {
            let mut out = format!(
                "No code matching pattern '{}' found.\n\
                 hints: use the codebase's own vocabulary (try search_memory first to \
                 discover it); shorter queries match more.",
                req.pattern_query
            );
            out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
            return Ok(CallToolResult::success(vec![Content::text(out)]));
        }

        // Per-file aggregate: hit count, best score, best snippet + line.
        let mut per_file: BTreeMap<String, (usize, f32, String, u32)> = BTreeMap::new();
        for h in hits.iter().take(PATTERN_LEXICAL_CAP) {
            let path = h.path.as_str().replace('\\', "/");
            if engram_core::is_vendor_path(&path) {
                continue;
            }
            let entry = per_file
                .entry(path)
                .or_insert((0, f32::MIN, String::new(), 0));
            entry.0 += 1;
            if h.score > entry.1 {
                entry.1 = h.score;
                entry.2 = h.snippet.clone().unwrap_or_default();
                entry.3 = h.start_line;
            }
        }
        coverage.lexical_files = per_file.len();

        // Kind filter (A1): a page query cannot have a script as exemplar.
        let mut candidates: Vec<(String, (usize, f32, String, u32))> =
            per_file.into_iter().collect();
        if kind != PatternKind::Any {
            let kept: Vec<_> = candidates
                .iter()
                .filter(|(p, _)| kind_matches(kind, p))
                .cloned()
                .collect();
            coverage.kind_matched_files = kept.len();
            if kept.is_empty() {
                coverage.failures.push(format!(
                    "no {} file among the {} lexical files — kind filter NOT applied, showing all kinds",
                    kind.as_str(),
                    candidates.len()
                ));
            } else {
                candidates = kept;
                coverage.kind_filter_applied = true;
            }
        }
        // Lexical pre-rank so the structural pass looks at the strongest N.
        candidates.sort_by(|a, b| {
            b.1.0.cmp(&a.1.0).then(
                b.1.1
                    .partial_cmp(&a.1.1)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
        });
        candidates.truncate(PATTERN_CANDIDATES);
        coverage.candidates_considered = candidates.len();

        // Structural pass (A3): shape from the graph + the .aspx on disk.
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let project_dir = self
            .state
            .registry
            .get_project(&req.project_id)
            .ok()
            .flatten()
            .map(|r| r.directory)
            .unwrap_or_default();
        let paths: Vec<String> = candidates.iter().map(|(p, _)| p.clone()).collect();
        let shapes: Vec<(String, ExemplarShape, Vec<String>, Vec<String>)> =
            tokio::task::spawn_blocking(move || {
                let root = std::path::PathBuf::from(project_dir);
                paths
                    .into_iter()
                    .map(|path| {
                        let mut failures = Vec::new();
                        let (shape, coupled) =
                            derive_exemplar_shape(&graph, &pid, &root, &path, &mut failures);
                        (path, shape, coupled, failures)
                    })
                    .collect()
            })
            .await
            .map_err(|e| McpError::internal_error(format!("shape pass panicked: {e}"), None))?;

        let mut ranked: Vec<PatternExemplar> = Vec::new();
        for ((path, (hits_n, score, snippet, line)), (_, shape, coupled, failures)) in
            candidates.into_iter().zip(shapes.into_iter())
        {
            coverage.failures.extend(failures);
            let structural = shape.structural_score();
            ranked.push(PatternExemplar {
                path: path.clone(),
                rank: 0,
                kind_match: kind_matches(kind, &path),
                hits: hits_n,
                score,
                structural,
                shape,
                coupled,
                snippet,
                line,
            });
        }
        // A2: structural fit first, then lexical evidence.
        ranked.sort_by(|a, b| {
            b.kind_match
                .cmp(&a.kind_match)
                .then(b.structural.cmp(&a.structural))
                .then(b.hits.cmp(&a.hits))
                .then(
                    b.score
                        .partial_cmp(&a.score)
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
        });
        let mut exemplars: Vec<PatternExemplar> = Vec::new();
        let mut seen_dirs: HashSet<String> = HashSet::new();
        let total = ranked.len();
        for ex in &ranked {
            if exemplars.len() >= max_examples {
                break;
            }
            let dir = ex
                .path
                .rsplit_once('/')
                .map(|(d, _)| d.to_string())
                .unwrap_or_default();
            if seen_dirs.contains(&dir) && total > max_examples {
                continue;
            }
            seen_dirs.insert(dir);
            exemplars.push(ex.clone());
        }
        for ex in &ranked {
            if exemplars.len() >= max_examples {
                break;
            }
            if !exemplars.iter().any(|e| e.path == ex.path) {
                exemplars.push(ex.clone());
            }
        }
        for (i, ex) in exemplars.iter_mut().enumerate() {
            ex.rank = i + 1;
        }

        // Common shapes: handler chains (bare method names) shared by ≥ 2 exemplars.
        let mut shape_counts: HashMap<String, usize> = HashMap::new();
        for ex in &exemplars {
            let mut seen: HashSet<&String> = HashSet::new();
            for chain in &ex.shape.handlers {
                if seen.insert(chain) {
                    *shape_counts.entry(common_shape_key(chain)).or_default() += 1;
                }
            }
        }
        let mut common_shapes: Vec<CommonShape> = shape_counts
            .into_iter()
            .filter(|(_, c)| *c >= 2)
            .map(|(shape, count)| CommonShape { shape, count })
            .collect();
        common_shapes.sort_by(|a, b| b.count.cmp(&a.count).then(a.shape.cmp(&b.shape)));

        let result = PatternJson {
            query: req.pattern_query.clone(),
            inferred_kind: kind.as_str().into(),
            exemplars,
            common_shapes,
            coverage,
        };
        if req.output_json {
            let text = serde_json::to_string_pretty(&result)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(text)]));
        }
        let mut out = render_pattern_markdown(&result);
        out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }
}

// ── find_implementation_pattern: kinds, shapes, coverage (row-5 audit) ────

/// Lexical candidates fetched (cap+1 for an honest status).
pub(crate) const PATTERN_LEXICAL_CAP: usize = 200;
/// Files that get the structural (graph) pass.
pub(crate) const PATTERN_CANDIDATES: usize = 15;
pub(crate) const PATTERN_HANDLERS_CAP: usize = 12;
pub(crate) const PATTERN_CONTROLS_CAP: usize = 30;
pub(crate) const PATTERN_DATA_CAP: usize = 12;
/// Handler chains follow `Calls` edges this deep (handler = depth 1).
pub(crate) const PATTERN_CHAIN_DEPTH: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PatternKind {
    Page,
    Class,
    Script,
    Any,
}

impl PatternKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Page => "page",
            Self::Class => "class",
            Self::Script => "script",
            Self::Any => "any",
        }
    }
}

/// What kind of exemplar the query asks for. Page words win over class
/// words (an "admin page … helper" is still a page); script words are
/// explicit.
pub(crate) fn infer_pattern_kind(query: &str) -> PatternKind {
    let q = query.to_ascii_lowercase();
    let has = |words: &[&str]| {
        q.split(|c: char| !c.is_ascii_alphanumeric())
            .any(|t| words.contains(&t))
    };
    if has(&[
        "page",
        "pages",
        "aspx",
        "ascx",
        "webform",
        "webforms",
        "usercontrol",
        "gridview",
        "postback",
        "codebehind",
        "code-behind",
        "listview",
        "repeater",
        "dropdownlist",
        // live G1 miss 2026-08-29: control-side words without "page"
        "control",
        "controls",
        "dropdown",
        "textbox",
        "checkbox",
        "listbox",
        "datagrid",
        "updatepanel",
        "master",
    ]) {
        PatternKind::Page
    } else if has(&[
        "typescript",
        "javascript",
        "script",
        "ts",
        "js",
        "jquery",
        "ajax",
        "dom",
    ]) {
        PatternKind::Script
    } else if has(&[
        "class",
        "helper",
        "service",
        "repository",
        "dal",
        "domain",
        "module",
        "api",
        "endpoint",
        "handler",
        "function",
    ]) {
        PatternKind::Class
    } else {
        PatternKind::Any
    }
}

pub(crate) fn kind_matches(kind: PatternKind, path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    let is_page = p.ends_with(".aspx.vb")
        || p.ends_with(".aspx.cs")
        || p.ends_with(".ascx.vb")
        || p.ends_with(".ascx.cs")
        || p.ends_with(".master.vb")
        || p.ends_with(".master.cs");
    match kind {
        PatternKind::Any => true,
        PatternKind::Page => is_page,
        PatternKind::Script => {
            (p.ends_with(".ts") || p.ends_with(".js") || p.ends_with(".tsx") || p.ends_with(".jsx"))
                && !p.ends_with(".d.ts")
        }
        PatternKind::Class => (p.ends_with(".vb") || p.ends_with(".cs")) && !is_page,
    }
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub(crate) struct ExemplarShape {
    /// Ordered handler chains: `Page_Load → BindGrid → GetAll` (by the
    /// handler's line), through `Calls` edges up to the depth cap.
    pub handlers: Vec<String>,
    pub handlers_total: usize,
    /// Server controls from the sibling .aspx/.ascx: `ID (Type)`.
    pub controls: Vec<String>,
    pub controls_total: usize,
    /// Data/state edges of the file's functions.
    pub data: Vec<String>,
    pub data_total: usize,
    pub chain_depth_cap_hit: bool,
}

impl ExemplarShape {
    pub(crate) fn structural_score(&self) -> usize {
        let chains_with_calls = self.handlers.iter().filter(|h| h.contains(" → ")).count();
        self.handlers_total * 3
            + chains_with_calls * 2
            + self.controls_total.min(10)
            + self.data_total.min(6)
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct PatternExemplar {
    pub path: String,
    pub rank: usize,
    pub kind_match: bool,
    pub hits: usize,
    pub score: f32,
    pub structural: usize,
    pub shape: ExemplarShape,
    pub coupled: Vec<String>,
    /// The FTS snippet — matched text, NOT the pattern.
    pub snippet: String,
    pub line: u32,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct CommonShape {
    pub shape: String,
    pub count: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct PatternCoverage {
    pub lexical_hits: usize,
    pub lexical_cap: usize,
    pub lexical_status: String,
    pub lexical_files: usize,
    pub kind_filter: String,
    pub kind_filter_applied: bool,
    pub kind_matched_files: usize,
    pub candidates_considered: usize,
    pub candidates_cap: usize,
    pub exemplar_cap: usize,
    pub handlers_cap: usize,
    pub controls_cap: usize,
    pub data_cap: usize,
    pub chain_depth_cap: usize,
    pub failures: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct PatternJson {
    pub query: String,
    pub inferred_kind: String,
    pub exemplars: Vec<PatternExemplar>,
    pub common_shapes: Vec<CommonShape>,
    pub coverage: PatternCoverage,
}

/// The house-pattern key of a handler chain: every target reduced to its
/// bare method name so `btnSave_Click → _rv.categories.Save → BindGrid` and
/// `btnSave_Click → _rv.units.Save → BindGrid` are the SAME shape.
pub(crate) fn common_shape_key(chain: &str) -> String {
    chain
        .split(" → ")
        .map(|step| {
            let mut names: Vec<&str> = step
                .split(" | ")
                .map(|t| t.rsplit('.').next().unwrap_or(t).trim())
                .collect();
            names.sort_unstable();
            names.dedup();
            names.join(" | ")
        })
        .collect::<Vec<_>>()
        .join(" → ")
}

const HANDLER_SUFFIXES: &[&str] = &[
    "Load",
    "Init",
    "PreRender",
    "Click",
    "Command",
    "SelectedIndexChanged",
    "TextChanged",
    "CheckedChanged",
    "RowDataBound",
    "RowCommand",
    "RowEditing",
    "RowUpdating",
    "RowDeleting",
    "RowCancelingEdit",
    "PageIndexChanging",
    "PageIndexChanged",
    "Sorting",
    "Selecting",
    "Inserting",
    "Inserted",
    "Updating",
    "Updated",
    "Deleting",
    "Deleted",
    "ItemCommand",
    "ItemDataBound",
    "DataBound",
    "ServerValidate",
    "Tick",
    "Unload",
];

pub(crate) fn is_handler_name(name: &str) -> bool {
    let bare = name.rsplit('.').next().unwrap_or(name);
    if bare.eq_ignore_ascii_case("Page_Load")
        || bare.eq_ignore_ascii_case("Page_Init")
        || bare.eq_ignore_ascii_case("Page_PreRender")
    {
        return true;
    }
    match bare.rsplit_once('_') {
        Some((_, suffix)) => HANDLER_SUFFIXES
            .iter()
            .any(|s| s.eq_ignore_ascii_case(suffix)),
        None => false,
    }
}

/// `sym:function:<path>:<Class.Method>:<line>` → (`Class.Method`, `Method`).
fn pattern_node_names(node_id: &str) -> (String, String) {
    // Dangling targets (`sym:function:::_rv.x.Save`) have empty segments;
    // a real id is `sym:function:<path>:<Class.Method>:<line>`.
    let parts: Vec<&str> = node_id.split(':').filter(|p| !p.is_empty()).collect();
    let qualified = match parts.len() {
        0 => node_id.to_string(),
        n if n >= 5 => parts[n - 2].to_string(),
        n => parts[n - 1].to_string(),
    };
    let bare = qualified
        .rsplit('.')
        .next()
        .unwrap_or(&qualified)
        .to_string();
    (qualified, bare)
}

/// Shape of one candidate file: handler chains through `Calls` edges,
/// controls from the sibling markup, data/state edges. Every provider
/// failure is a named line, never an empty section.
pub(crate) fn derive_exemplar_shape(
    graph: &std::sync::Arc<engram_graph::GraphStore>,
    pid: &str,
    root: &std::path::Path,
    path: &str,
    failures: &mut Vec<String>,
) -> (ExemplarShape, Vec<String>) {
    let mut shape = ExemplarShape::default();
    let nodes = match graph.query_nodes(pid, None, None, Some(path), 500) {
        Ok(n) => n,
        Err(e) => {
            failures.push(format!("{path}: graph node query failed: {e}"));
            Vec::new()
        }
    };
    let mut functions: Vec<&engram_graph::Node> =
        nodes.iter().filter(|n| n.node_type == "function").collect();
    functions.sort_by_key(|n| n.start_line);
    let in_file: HashSet<&str> = functions.iter().map(|n| n.node_id.as_str()).collect();
    let by_id: HashMap<&str, &engram_graph::Node> =
        functions.iter().map(|n| (n.node_id.as_str(), *n)).collect();
    let by_bare: HashMap<String, &str> = functions
        .iter()
        .map(|n| {
            let bare = n
                .name
                .rsplit('.')
                .next()
                .unwrap_or(&n.name)
                .to_ascii_lowercase();
            (bare, n.node_id.as_str())
        })
        .collect();
    let source_lines: Vec<String> = match std::fs::read_to_string(root.join(path)) {
        Ok(t) => t.lines().map(|l| l.to_string()).collect(),
        Err(e) => {
            failures.push(format!(
                "{path}: unreadable — in-file call chains come from the graph only: {e}"
            ));
            Vec::new()
        }
    };
    // Bare in-class calls in a node's body → sibling function node ids.
    let textual_callees = |id: &str| -> Vec<String> {
        static RE_BARE_CALL: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(r"(?:^|[^.\w])([A-Za-z_]\w*)\s*\(").expect("RE_BARE_CALL")
        });
        let Some(n) = by_id.get(id) else {
            return Vec::new();
        };
        let start = (n.start_line.max(1) - 1) as usize;
        let end = (n.end_line as usize).min(source_lines.len());
        if start >= end {
            return Vec::new();
        }
        let mut out = Vec::new();
        for line in &source_lines[start..end] {
            let code = line.split('\'').next().unwrap_or(line);
            for cap in RE_BARE_CALL.captures_iter(code) {
                let callee = cap[1].to_ascii_lowercase();
                if let Some(target) = by_bare.get(&callee)
                    && *target != id
                {
                    out.push((*target).to_string());
                }
            }
        }
        out
    };

    let handlers: Vec<&engram_graph::Node> = functions
        .iter()
        .copied()
        .filter(|n| is_handler_name(&n.name))
        .collect();
    shape.handlers_total = handlers.len();
    for h in handlers.iter().take(PATTERN_HANDLERS_CAP) {
        let (_, bare) = pattern_node_names(&h.node_id);
        let mut chain: Vec<String> = vec![bare];
        let mut frontier: Vec<String> = vec![h.node_id.clone()];
        let mut visited: HashSet<String> = HashSet::new();
        visited.insert(h.node_id.clone());
        for depth in 2..=PATTERN_CHAIN_DEPTH + 1 {
            let mut next: Vec<String> = Vec::new();
            for id in &frontier {
                match graph.neighbors(pid, EdgeKind::Calls, id, 20) {
                    Ok(neigh) => {
                        for (target, _) in neigh {
                            if visited.insert(target.clone()) {
                                next.push(target);
                            }
                        }
                    }
                    Err(e) => failures.push(format!("{path}: calls of {id} failed: {e}")),
                }
                if in_file.contains(id.as_str()) {
                    for target in textual_callees(id) {
                        if visited.insert(target.clone()) {
                            next.push(target);
                        }
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            if depth > PATTERN_CHAIN_DEPTH {
                shape.chain_depth_cap_hit = true;
                break;
            }
            let mut names: Vec<String> = next
                .iter()
                .map(|t| {
                    let (qualified, bare) = pattern_node_names(t);
                    if in_file.contains(t.as_str()) {
                        bare
                    } else {
                        qualified
                    }
                })
                .collect();
            names.sort();
            names.dedup();
            chain.push(names.join(" | "));
            frontier = next;
        }
        shape.handlers.push(chain.join(" → "));
    }

    // Controls from the sibling markup (the extractor's page model parses
    // the same file; a read failure is a provider failure).
    let lower = path.to_ascii_lowercase();
    let markup = if lower.ends_with(".vb") || lower.ends_with(".cs") {
        let cut = path.len() - 3;
        let m = &path[..cut];
        let ml = m.to_ascii_lowercase();
        if ml.ends_with(".aspx") || ml.ends_with(".ascx") || ml.ends_with(".master") {
            Some(m.to_string())
        } else {
            None
        }
    } else {
        None
    };
    if let Some(m) = markup {
        match std::fs::read_to_string(root.join(&m)) {
            Ok(text) => {
                static RE_CONTROL: std::sync::LazyLock<regex::Regex> =
                    std::sync::LazyLock::new(|| {
                        regex::Regex::new(r#"(?i)<asp:(\w+)\s[^>]*?\bID\s*=\s*"(\w+)""#)
                            .expect("RE_CONTROL")
                    });
                let mut all: Vec<String> = RE_CONTROL
                    .captures_iter(&text)
                    .map(|c| format!("{} ({})", &c[2], &c[1]))
                    .collect();
                shape.controls_total = all.len();
                all.truncate(PATTERN_CONTROLS_CAP);
                shape.controls = all;
            }
            Err(e) => failures.push(format!("{path}: markup {m} unreadable: {e}")),
        }
    }

    // Data/state edges of the file's functions.
    let mut data: Vec<String> = Vec::new();
    for n in &functions {
        for kind in [
            EdgeKind::SqlCalls,
            EdgeKind::QueriesTable,
            EdgeKind::ReadsState,
            EdgeKind::WritesState,
        ] {
            match graph.neighbors(pid, kind.clone(), &n.node_id, 10) {
                Ok(neigh) => {
                    for (target, _) in neigh {
                        data.push(format!("[{}] {target}", kind.as_str()));
                    }
                }
                Err(e) => failures.push(format!(
                    "{path}: {} edges of {} failed: {e}",
                    kind.as_str(),
                    n.name
                )),
            }
        }
    }
    data.sort();
    data.dedup();
    shape.data_total = data.len();
    data.truncate(PATTERN_DATA_CAP);
    shape.data = data;

    let file_node = format!("file:{path}");
    let coupled: Vec<String> = match graph.neighbors(pid, EdgeKind::TemporalCoupling, &file_node, 3)
    {
        Ok(v) => v
            .into_iter()
            .map(|(id, w)| format!("{} (co-changed {w}x)", id.trim_start_matches("file:")))
            .collect(),
        Err(e) => {
            failures.push(format!("{path}: co-change lookup failed: {e}"));
            Vec::new()
        }
    };
    (shape, coupled)
}

pub(crate) fn render_pattern_markdown(r: &PatternJson) -> String {
    let cov = &r.coverage;
    let mut out = format!("# Implementation pattern exemplars: '{}'\n", r.query);
    out.push_str(&format!(
        "inferred kind: {}{}\n",
        r.inferred_kind,
        if cov.kind_filter_applied {
            format!(
                " (filter applied: {} of {} lexical files are {} files)",
                cov.kind_matched_files, cov.lexical_files, r.inferred_kind
            )
        } else {
            String::new()
        }
    ));
    for ex in &r.exemplars {
        out.push_str(&format!(
            "\n## Exemplar #{}: {} ({} match(es), score {:.2}, structural {})\n",
            ex.rank, ex.path, ex.hits, ex.score, ex.structural
        ));
        if ex.shape.handlers.is_empty() {
            out.push_str("shape: no event handlers found in the graph for this file\n");
        } else {
            out.push_str(&format!(
                "shape — handler chains ({} of {}, depth ≤ {}{}):\n",
                ex.shape.handlers.len(),
                ex.shape.handlers_total,
                cov.chain_depth_cap,
                if ex.shape.chain_depth_cap_hit {
                    ", depth cap hit"
                } else {
                    ""
                }
            ));
            for h in &ex.shape.handlers {
                out.push_str(&format!("- {h}\n"));
            }
        }
        if !ex.shape.controls.is_empty() {
            out.push_str(&format!(
                "controls ({} of {}): {}\n",
                ex.shape.controls.len(),
                ex.shape.controls_total,
                ex.shape.controls.join(", ")
            ));
        }
        if !ex.shape.data.is_empty() {
            out.push_str(&format!(
                "data/state ({} of {}): {}\n",
                ex.shape.data.len(),
                ex.shape.data_total,
                ex.shape.data.join("; ")
            ));
        }
        if !ex.coupled.is_empty() {
            out.push_str(&format!("co-changes with: {}\n", ex.coupled.join("; ")));
        }
        if !ex.snippet.is_empty() {
            let trimmed: String = ex.snippet.chars().take(600).collect();
            out.push_str(&format!(
                "matched text (FTS snippet, line {} — evidence of the match, not the pattern):\n```\n{trimmed}\n```\n",
                ex.line
            ));
        }
    }
    if r.common_shapes.is_empty() {
        out.push_str("\n## Common shapes\n_No handler chain is shared by ≥ 2 exemplars — no house pattern can be claimed from these files._\n");
    } else {
        out.push_str("\n## Common shapes (in ≥ 2 exemplars — the house pattern)\n");
        for c in &r.common_shapes {
            out.push_str(&format!("- {} ({}x)\n", c.shape, c.count));
        }
    }
    out.push_str("\n## Coverage\n");
    out.push_str(&format!(
        "- lexical: {} ({} hits, cap {}; {} files)\n",
        cov.lexical_status, cov.lexical_hits, cov.lexical_cap, cov.lexical_files
    ));
    out.push_str(&format!(
        "- kind filter: {} ({})\n",
        cov.kind_filter,
        if cov.kind_filter_applied {
            "applied"
        } else {
            "not applied"
        }
    ));
    out.push_str(&format!(
        "- structural pass: {} candidate(s) (cap {}); exemplars cap {}; handlers cap {}; controls cap {}; data cap {}; chain depth cap {}\n",
        cov.candidates_considered,
        cov.candidates_cap,
        cov.exemplar_cap,
        cov.handlers_cap,
        cov.controls_cap,
        cov.data_cap,
        cov.chain_depth_cap
    ));
    if cov.failures.is_empty() {
        out.push_str("- failures: none\n");
    } else {
        for f in &cov.failures {
            out.push_str(&format!("- FAILURE: {f}\n"));
        }
    }
    out.push_str(
        "\nnext: get_full_method_body on the best exemplar's handlers to imitate the chain; \
         get_page_context on its .aspx for the control wiring; get_concept_footprint for the \
         domain concept you're wiring in.\n",
    );
    out
}

#[cfg(test)]
mod implementation_pattern_unit_tests {
    use super::*;

    #[test]
    fn kind_inference_prefers_page_words() {
        assert_eq!(
            infer_pattern_kind("admin page with a GridView and a save button"),
            PatternKind::Page
        );
        // Live miss (OciusX G1, 2026-08-29): "user control" / "dropdown" are
        // page-side words too.
        assert_eq!(
            infer_pattern_kind(
                "user control with a dropdown bound to a lookup table and a search button"
            ),
            PatternKind::Page
        );
        assert_eq!(
            infer_pattern_kind("helper class that validates input"),
            PatternKind::Class
        );
        assert_eq!(
            infer_pattern_kind("typescript quantity manager"),
            PatternKind::Script
        );
        assert_eq!(
            infer_pattern_kind("something else entirely"),
            PatternKind::Any
        );
    }

    #[test]
    fn kind_matching_by_path() {
        assert!(kind_matches(PatternKind::Page, "Site/a/b.aspx.vb"));
        assert!(!kind_matches(PatternKind::Page, "Site/ts/x.ts"));
        assert!(!kind_matches(PatternKind::Class, "Site/a/b.aspx.vb"));
        assert!(kind_matches(PatternKind::Class, "Site/App_Code/x.vb"));
        assert!(kind_matches(PatternKind::Script, "Site/ts/x.ts"));
        assert!(!kind_matches(PatternKind::Script, "Site/ts/x.d.ts"));
    }

    #[test]
    fn common_shape_key_drops_class_qualifiers() {
        assert_eq!(
            common_shape_key("btnSave_Click → _rv.categories.Save | BindGrid → GetAll"),
            "btnSave_Click → BindGrid | Save → GetAll"
        );
        assert_eq!(
            common_shape_key("btnSave_Click → _rv.units.Save | BindGrid → GetAll"),
            common_shape_key("btnSave_Click → _rv.categories.Save | BindGrid → GetAll")
        );
    }

    #[test]
    fn handler_names() {
        assert!(is_handler_name("Page_Load"));
        assert!(is_handler_name("system_project_project.btnSok_Click"));
        assert!(is_handler_name("ddlType_SelectedIndexChanged"));
        assert!(!is_handler_name("BindTypeDropDown"));
        assert!(!is_handler_name("GetAll"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concept_stems_cover_plural_and_compound() {
        let stems = concept_stems("Code Categories");
        assert!(stems.contains(&"code categories".to_string()));
        assert!(stems.contains(&"code categorie".to_string())); // naive singular
        assert!(stems.contains(&"codecategories".to_string())); // compact
        assert!(matches_concept("ddlCodeCategories", &stems));
        assert!(matches_concept("CODE_CATEGORIES", &stems));
    }

    #[test]
    fn matches_concept_ignores_case_and_separators() {
        let stems = concept_stems("photo");
        assert!(matches_concept("PhotoCount", &stems));
        assert!(matches_concept("MIN_PHOTOS_REQUIRED", &stems));
        assert!(matches_concept("tbl_photos", &stems));
        assert!(!matches_concept("OrderStatus", &stems));
    }

    #[test]
    fn path_token_bag_extracts_shape_tokens() {
        let bag = path_token_bag(&["Admin/CodeCategories.aspx.cs".to_string()]);
        assert!(bag.contains("dir:admin"));
        assert!(bag.contains("ext:cs"));
        assert!(bag.contains("ext:aspx.cs"));
        assert!(bag.contains("w:codecategories"));
    }

    #[test]
    fn bag_jaccard_orders_similarity_sensibly() {
        let plan = path_token_bag(&[
            "PhotoSettings.aspx".to_string(),
            "PhotoSettings.aspx.cs".to_string(),
        ]);
        let similar = path_token_bag(&[
            "OrderSettings.aspx".to_string(),
            "OrderSettings.aspx.cs".to_string(),
            "Admin/menu.xml".to_string(),
        ]);
        let unrelated = path_token_bag(&["Scripts/jquery.min.js".to_string()]);
        assert!(bag_jaccard(&plan, &similar) > bag_jaccard(&plan, &unrelated));
        assert_eq!(bag_jaccard(&plan, &HashSet::new()), 0.0);
    }

    #[test]
    fn dir_ext_shape_reports_pattern() {
        assert_eq!(
            dir_ext_shape("Admin/Users.aspx").as_deref(),
            Some("Admin/*.aspx")
        );
        assert_eq!(dir_ext_shape("menu.xml").as_deref(), Some("*.xml"));
        assert_eq!(dir_ext_shape("Makefile"), None);
    }

    #[test]
    fn change_set_paths_keeps_orm_model_files() {
        // External audit 2026-08-29 P0-3: the footprint named `iFalt.dbml` and
        // the candidate parser dropped it — the LINQ-to-SQL / EF model files
        // must survive the extension alternation like any code file.
        let v = change_set_paths(
            "touch Site/App_Code/iFalt.dbml and Models/Ocius.edmx next to Site/x.vb",
        );
        assert!(v.contains(&"site/app_code/ifalt.dbml".to_string()), "{v:?}");
        assert!(v.contains(&"models/ocius.edmx".to_string()), "{v:?}");
        assert!(v.contains(&"site/x.vb".to_string()), "{v:?}");
    }

    #[test]
    fn change_set_paths_captures_build_output_dirs() {
        // A build-output directory named `~.js/` itself ends in a code
        // extension. The extractor must greedily capture the FULL file, not
        // truncate to the directory — the bug that silently dropped co-change's
        // strongest partner (map.js) from every change set.
        let p =
            change_set_paths("- `modules/map/~.js/map.js` (45 co-changes with `Site/x/map.aspx`)");
        assert!(p.contains(&"modules/map/~.js/map.js".to_string()), "{p:?}");
        assert!(p.contains(&"site/x/map.aspx".to_string()), "{p:?}");
        // `.css` is a recognized extension and survives a `~.css/` dir.
        assert_eq!(
            change_set_paths("- modules/map/~.css/map.css"),
            vec!["modules/map/~.css/map.css".to_string()]
        );
        // Compound extensions still resolve to the longest valid match.
        assert_eq!(
            change_set_paths("foo/page.aspx.vb changed"),
            vec!["foo/page.aspx.vb".to_string()]
        );
        // A trailing sentence period must not be swallowed into the path.
        assert_eq!(
            change_set_paths("see modules/foo.cs."),
            vec!["modules/foo.cs".to_string()]
        );
        // A bare filename with no directory is still dropped (keep filter).
        assert!(change_set_paths("map.js").is_empty());
        // OpenAPI/config specs (yaml/yml) are recognized so co-change can surface
        // them (e.g. an API spec that co-changes with its controllers). json is
        // deliberately NOT recognized — package.json/tsconfig.json co-change with
        // too much to be useful signal.
        assert_eq!(
            change_set_paths("- `docs/openapi/ox-fiber.yaml`"),
            vec!["docs/openapi/ox-fiber.yaml".to_string()]
        );
    }

    #[test]
    fn concept_stems_plural_matches_singular_identifiers() {
        // A plural concept must match singular code identifiers (the PR1913 gap:
        // concept "roqentries" vs files RoqEntryService / RoqEntryDto).
        let stems = concept_stems("roqentries");
        assert!(stems.contains(&"roqentry".to_string()), "{stems:?}");
        assert!(matches_concept("RoqEntryService", &stems));
        assert!(matches_concept("RoqEntriesController", &stems)); // plural still matches
        // categories -> category
        assert!(concept_stems("categories").contains(&"category".to_string()));
        // a singular concept is unaffected (no bogus stem corrupts matching).
        assert!(matches_concept("InvoiceStatus", &concept_stems("invoice")));
    }

    #[test]
    fn story_concepts_prefer_distinctive_identifiers_over_crud_verbs() {
        // PR1913 shape: the narrative preamble is generic CRUD ("update ... status"),
        // the high-signal token is the API resource named in the endpoint path.
        // Generic CRUD verbs must not crowd it out of the top-3 concepts.
        // Real-shape: prose preamble BEFORE the endpoint path, so document-order
        // alone would pick invoice/status/customer and never reach the resource.
        let story = "As an admin I would like to be able to update RoQ invoice status \
                     from the API. The customer pulls the RoQ data every night from their \
                     own system and now wants to update the entries invoice status. We \
                     handle this as a specific command rather than a CRUD endpoint. \
                     POST api/v2/roqentries/{id}/setasbilled. Only admins may call it.";
        let c = extract_story_concepts(story);
        assert!(
            c.contains(&"roqentries".to_string()),
            "endpoint-path resource must surface despite being buried: {c:?}"
        );
        assert!(
            !c.contains(&"update".to_string()),
            "generic CRUD verb must be demoted: {c:?}"
        );
    }

    #[test]
    fn story_concepts_add_auth_concept_when_permission_gated() {
        // PR1913 shape: "Restricted to the Administrator role" — the auth layer
        // (role/user model) is the most-missed file. "role" surfaces it but sits
        // past the top-3 cutoff; the supplement must add it without displacing
        // the domain concepts.
        let c = extract_story_concepts(
            "POST api/v2/roqentries/{id}/setasbilled. Restricted to the Administrator role.",
        );
        assert!(
            c.contains(&"roqentries".to_string()),
            "domain concept kept: {c:?}"
        );
        assert!(c.contains(&"role".to_string()), "auth concept added: {c:?}");

        // Other auth phrasings also trigger.
        assert!(
            extract_story_concepts("the endpoint requires the Export permission")
                .contains(&"role".to_string())
        );
        assert!(
            extract_story_concepts("only authorized managers can approve")
                .contains(&"role".to_string())
        );

        // No auth language -> no auth concept (no false trigger / noise).
        let plain = extract_story_concepts("Show the invoice filter form on the report page");
        assert!(
            !plain.contains(&"role".to_string()),
            "no spurious auth concept: {plain:?}"
        );
    }

    #[test]
    fn story_concepts_reject_hash_and_id_garbage() {
        // PR1938 shape: a commit SHA + a board/username ID leaked into the story
        // and stole the concept slots, tanking recall. They must be rejected so
        // the real domain tokens surface.
        let c = extract_story_concepts(
            "acmeorg0375 a778c06a field worker searches the RoQ code list by redovisning category",
        );
        assert!(
            !c.contains(&"a778c06a".to_string()),
            "commit hash must be rejected: {c:?}"
        );
        assert!(
            !c.contains(&"acmeorg0375".to_string()),
            "username/id must be rejected: {c:?}"
        );
        // Garbage is gone; real word tokens fill the slots instead.
        assert!(
            !c.is_empty()
                && c.iter()
                    .all(|t| t.chars().filter(char::is_ascii_digit).count() < 3),
            "no 3+-digit garbage among concepts: {c:?}"
        );
        // Tokens with <3 digits (e.g. an api version) are still allowed.
        assert!(extract_story_concepts("update the apiv2 endpoint").contains(&"apiv2".to_string()));
    }

    #[test]
    fn transpile_pair_candidates_links_ts_and_committed_js() {
        // .ts source -> same-dir .js AND the ts/ -> ~.js/ output-dir swap.
        let c = transpile_pair_candidates("modules/map/ts/map.ts");
        assert!(c.contains(&"modules/map/ts/map.js".to_string()), "{c:?}");
        assert!(c.contains(&"modules/map/~.js/map.js".to_string()), "{c:?}");
        assert!(c.contains(&"modules/map/js/map.js".to_string()), "{c:?}");
        // reverse: committed bundle -> its .ts source (edit-the-bundle case).
        let r = transpile_pair_candidates("modules/map/~.js/map.js");
        assert!(r.contains(&"modules/map/ts/map.ts".to_string()), "{r:?}");
        assert!(r.contains(&"modules/map/~.js/map.ts".to_string()), "{r:?}");
        // .tsx/.jsx handled; basename preserved (incl. dotted stems).
        assert!(
            transpile_pair_candidates("a/b/Grid.view.tsx")
                .contains(&"a/b/Grid.view.js".to_string())
        );
        // non-TS/JS paths yield nothing (no false pairing for .json/.css/.vb).
        assert!(transpile_pair_candidates("a/b/config.json").is_empty());
        assert!(transpile_pair_candidates("a/b/page.aspx.vb").is_empty());
    }

    #[test]
    fn interface_pair_candidates_links_service_and_iservice() {
        use super::interface_pair_candidates;
        // Implementation -> interface, same dir and interfaces/ subfolder
        // (the live PR1913 miss shape).
        let c = interface_pair_candidates(
            "app_code/api-v2/services/reportingofquantities/roqentryservice.vb",
        );
        assert!(
            c.contains(
                &"app_code/api-v2/services/reportingofquantities/interfaces/iroqentryservice.vb"
                    .to_string()
            ),
            "{c:?}"
        );
        assert!(
            c.contains(
                &"app_code/api-v2/services/reportingofquantities/iroqentryservice.vb".to_string()
            ),
            "{c:?}"
        );
        // Interface -> implementation, out of the interfaces/ folder.
        let r = interface_pair_candidates(
            "app_code/api-v2/services/reportingofquantities/interfaces/iroqentryservice.vb",
        );
        assert!(
            r.contains(
                &"app_code/api-v2/services/reportingofquantities/roqentryservice.vb".to_string()
            ),
            "{r:?}"
        );
        // Markup code-behind and designer files pair via the page rule, not here.
        assert!(interface_pair_candidates("a/b/page.aspx.vb").is_empty());
        assert!(interface_pair_candidates("a/b/form.designer.cs").is_empty());
        // Root-level file (no dir) yields nothing.
        assert!(interface_pair_candidates("standalone.vb").is_empty());
    }

    #[test]
    fn can_helper_names_match_convention_only() {
        use super::is_can_helper_name;
        assert!(is_can_helper_name("CanUserUploadMarkerIcon"));
        assert!(is_can_helper_name("aspnetUsers.CanEditRoq"));
        assert!(is_can_helper_name("CanDo"));
        // Not permission helpers: Cancel/Canonical/scan/short names.
        assert!(!is_can_helper_name("Cancel"));
        assert!(!is_can_helper_name("CancelOrder"));
        assert!(!is_can_helper_name("Canonicalize"));
        assert!(!is_can_helper_name("ScanFiles"));
        assert!(!is_can_helper_name("Can"));
        assert!(!is_can_helper_name("candidate_list"));
    }

    #[test]
    fn api_spec_docs_finds_contract_documents() {
        use super::{api_spec_docs, is_api_code_path};
        let index: Vec<String> = [
            "docs/openapi/ox-fiber.yaml",
            "docs/openapi/ox-core.yaml",
            "app_code/api-v2/controllers/roqentriescontroller.vb",
            "node_modules/swagger-ui/dist/swagger-ui.json", // vendor → excluded
            "docs/readme.md",                               // not a spec
            "config/swagger.json",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let specs = api_spec_docs(&index);
        assert_eq!(
            specs,
            vec![
                "config/swagger.json".to_string(),
                "docs/openapi/ox-core.yaml".to_string(),
                "docs/openapi/ox-fiber.yaml".to_string(),
            ]
        );
        // API-layer detection: api-ish path segment + code extension.
        assert!(is_api_code_path(
            "app_code/api-v2/controllers/roqentriescontroller.vb"
        ));
        assert!(is_api_code_path(
            "app_code/installationsobjekt/api-json/x.vb"
        ));
        // Not API code: no api segment, or non-code files.
        assert!(!is_api_code_path("modules/dashboard/pages/map.aspx.vb"));
        assert!(!is_api_code_path("docs/openapi/ox-fiber.yaml"));
        // "apiary-docs-archive" style long segments do not count.
        assert!(!is_api_code_path("apiary-docs-archive/util.vb"));
    }

    #[test]
    fn scaffold_stem_groups_a_feature_cohort() {
        // All files of one api-v2 feature must reduce to the same entity stem so
        // the cohort groups together. (RoqEntriesController plural, RoqEntry-In/-Out
        // DTOs, IRoqEntryService interface.)
        for f in [
            "RoqEntriesController",
            "RoqEntryService",
            "IRoqEntryService",
            "RoqEntry-In",
            "RoqEntry-Out",
            "RoqEntryQuery",
        ] {
            assert_eq!(scaffold_stem(f), "roqentry", "{f}");
        }
        // Leading-I is only stripped for an interface (I + Uppercase), not real words.
        assert_eq!(scaffold_stem("InstallationPlan"), "installationplan");
    }

    #[test]
    fn derive_scaffold_entity_prefers_index_grounded_ngram() {
        let index = vec![
            "modules/dashboard/pages/public/markerinspection/markerinspection.aspx.vb".to_string(),
            "App_Code/api-v2/Controllers/timeReporting/TimeReportEntriesController.vb".to_string(),
        ];
        let got = super::derive_scaffold_entity(
            "Add a REST API endpoint to expose marker inspection statuses",
            &index,
        );
        assert_eq!(got.as_deref(), Some("MarkerInspection"));
        // No grounded match → None (caller falls back to concepts).
        assert_eq!(
            super::derive_scaffold_entity("Add an API for flux capacitors", &index),
            None
        );
        // Pass 2 (the real PR1890 shape): the story names the domain
        // indirectly — the page DIR is markerinspection, the story only
        // says "inspection". Needs ≥2 files under the dir.
        let index2 = vec![
            "modules/dashboard/pages/public/markerinspection/markerinspection.aspx".to_string(),
            "modules/dashboard/pages/public/markerinspection/markerinspection.aspx.vb".to_string(),
        ];
        let got2 = super::derive_scaffold_entity(
            "As a user I would like an inspection report API endpoint like the Inspection module",
            &index2,
        );
        assert_eq!(got2.as_deref(), Some("MarkerInspection"));
    }

    #[test]
    fn propose_scaffold_paths_parameterizes_template() {
        let cohort = vec![
            "App_Code/api-v2/Controllers/timeReporting/TimeReportEntriesController.vb".to_string(),
            "App_Code/api-v2/Services/timeReporting/TimeReportEntryService.vb".to_string(),
            "App_Code/api-v2/Services/timeReporting/interfaces/ITimeReportEntryService.vb"
                .to_string(),
            "App_Code/api-v2/QueryParams/timeReporting/TimeReportEntryQuery.vb".to_string(),
            "App_Code/api-v2/DataTransferObjects/timeReporting/TimeReportEntry-Out.vb".to_string(),
        ];
        let got = super::propose_scaffold_paths(&cohort, "MarkerInspection");
        assert!(
            got.contains(
                &"App_Code/api-v2/Controllers/markerInspection/MarkerInspectionsController.vb"
                    .to_string()
            ),
            "{got:?}"
        );
        assert!(got.contains(
            &"App_Code/api-v2/Services/markerInspection/MarkerInspectionService.vb".to_string()
        ));
        assert!(
            got.contains(
                &"App_Code/api-v2/Services/markerInspection/interfaces/IMarkerInspectionService.vb"
                    .to_string()
            )
        );
        assert!(got.contains(
            &"App_Code/api-v2/QueryParams/markerInspection/MarkerInspectionQuery.vb".to_string()
        ));
        assert!(
            got.contains(
                &"App_Code/api-v2/DataTransferObjects/markerInspection/MarkerInspection-Out.vb"
                    .to_string()
            )
        );
    }

    #[test]
    fn find_analog_cohort_picks_a_complete_feature() {
        // Original-case paths (as the index provides them) — capital I matters.
        let index: Vec<String> = [
            "App_Code/api-v2/Controllers/Roq/RoqEntriesController.vb",
            "App_Code/api-v2/Services/Roq/RoqEntryService.vb",
            "App_Code/api-v2/Services/Roq/interfaces/IRoqEntryService.vb",
            "App_Code/api-v2/DataTransferObjects/Roq/RoqEntry-In.vb",
            "App_Code/api-v2/QueryParams/Roq/RoqEntryQuery.vb",
            // an unrelated, shallow group (should not win)
            "App_Code/grunddata/code/Projekt.vb",
            "App_Code/grunddata/code/Detaljer.vb",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let none = std::collections::HashSet::new();
        let cohort = find_analog_cohort(&index, "controller", &none).expect("cohort");
        let lc: Vec<String> = cohort.iter().map(|f| f.to_lowercase()).collect();
        assert!(
            lc.iter().any(|f| f.contains("roqentriescontroller")),
            "{cohort:?}"
        );
        assert!(
            lc.iter().any(|f| f.contains("iroqentryservice")),
            "interface dropped: {cohort:?}"
        );
        assert!(cohort.len() >= 5, "{cohort:?}");
        // cue filter: a cue absent from any cohort yields nothing.
        assert!(find_analog_cohort(&index, "nonexistentcue", &none).is_none());
    }

    #[test]
    fn analog_cohort_prefers_the_story_connected_family() {
        // TWO equally-complete api families; the ranked candidate set
        // overlaps only the Marker one — it must win DETERMINISTICALLY
        // (the old map-order tie made template picks flip across runs).
        let index: Vec<String> = [
            "App_Code/api-v2/Controllers/Roq/RoqEntriesController.vb",
            "App_Code/api-v2/Services/Roq/RoqEntryService.vb",
            "App_Code/api-v2/Services/Roq/interfaces/IRoqEntryService.vb",
            "App_Code/api-v2/DataTransferObjects/Roq/RoqEntry-In.vb",
            "App_Code/api-v2/Controllers/Marker/MarkersController.vb",
            "App_Code/api-v2/Services/Marker/MarkerService.vb",
            "App_Code/api-v2/Services/Marker/interfaces/IMarkerService.vb",
            "App_Code/api-v2/DataTransferObjects/Marker/Marker-In.vb",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let ranked: std::collections::HashSet<String> =
            ["app_code/api-v2/services/marker/markerservice.vb".to_string()]
                .into_iter()
                .collect();
        let cohort = find_analog_cohort(&index, "controller", &ranked).expect("cohort");
        assert!(
            cohort.iter().any(|f| f.contains("MarkersController")),
            "story-connected family must win: {cohort:?}"
        );
        // With NO overlap on either side, the lexicographic tiebreak makes
        // the pick stable (Marker < Roq alphabetically).
        let none = std::collections::HashSet::new();
        let c1 = find_analog_cohort(&index, "controller", &none).expect("cohort");
        let c2 = find_analog_cohort(&index, "controller", &none).expect("cohort");
        assert_eq!(c1, c2);
        assert!(c1.iter().any(|f| f.contains("MarkersController")), "{c1:?}");
    }
}

// ── map_guards_and_settings + plan_user_story ────────────────────────────────

/// Settings-shaped table names: tables that store configuration rows.
// ── map_guards_and_settings: three-state verdicts, helper credit, coverage ─

/// Functions / helper names printed per markdown list before "… and N more".
pub(crate) const GUARD_LIST_CAP: usize = 25;
pub(crate) const GUARD_SETTINGS_FN_CAP: usize = 300;
pub(crate) const GUARD_SETTINGS_EDGE_CAP: usize = 20;
pub(crate) const GUARD_SETTINGS_TABLE_CAP: usize = 10;
pub(crate) const GUARD_HELPER_HOP_CAP: usize = 50;
pub(crate) const GUARD_HOUSE_CAP: usize = 8;
pub(crate) const GUARD_ROLES_CAP: usize = 10;

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct GuardVerdict {
    pub name: String,
    pub file: String,
    pub line: u32,
    /// `guarded` | `unguarded` | `unknown`
    pub verdict: String,
    /// The guard family/checks credited (`CheckRead;CheckWrite`).
    pub family: String,
    /// `role` for the check families the extractor recognises; object /
    /// tenant scoping is not detected yet (row-8 A2) and is reported so.
    pub level: Option<String>,
    pub roles: String,
    /// The helper the guard was inherited from (one hop through Calls).
    pub via: Option<String>,
    pub reason: String,
    /// Scope keys read from CLIENT input in the body (`qry.params("pr_id")`,
    /// `Request("pr_id")`, `GetDictionary…Value(qry.params, "pr_id")`).
    pub scope_reads: Vec<String>,
    /// Reads a client scope key and no object-level guard covers it —
    /// the role check alone does not scope the data (row-8 A2).
    pub role_only: bool,
    /// Every own check sits inside a branch (`If x Then … Check… End If`)
    /// — it does not guard every path; a helper that guards
    /// unconditionally is credited instead when one is called (row-8 D8).
    pub own_check_conditional: bool,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub(crate) struct GuardsCoverage {
    /// `complete` | `truncated` — the project-wide function scan that feeds
    /// the house patterns.
    pub node_scan: String,
    pub scanned: usize,
    pub node_scan_cap: usize,
    /// `store` (file/path scope queried at the store), `filter` (name scope
    /// or store miss ⇒ substring filter over the scan), `none`.
    pub scope_query: String,
    pub in_scope_functions: usize,
    pub caps: Vec<String>,
    pub failures: Vec<String>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub(crate) struct GuardsReport {
    pub scope: Option<String>,
    pub functions: Vec<GuardVerdict>,
    pub guarded: Vec<String>,
    pub unguarded: Vec<String>,
    pub unknown: Vec<String>,
    /// Guarded functions whose role check does not cover the client
    /// scope key they read (A2).
    pub role_only: Vec<String>,
    pub house_patterns: Vec<(String, usize)>,
    pub roles_seen: Vec<(String, usize)>,
    pub settings_read: Vec<(String, Vec<String>)>,
    pub settings_tables: Vec<(String, usize)>,
    pub app_settings_defined: usize,
    pub coverage: GuardsCoverage,
}

fn node_meta_str<'a>(n: &'a engram_graph::Node, key: &str) -> &'a str {
    n.metadata
        .as_ref()
        .and_then(|m| m.get(key))
        .and_then(|v| v.as_str())
        .unwrap_or("")
}

/// `object` when the family scopes an entity by its id (`check_pr_id`,
/// `check_<x>_id`, `CheckAccess…`, `HasAccessTo…`, `CanAccess…`,
/// `IsOwnerOf…`) — live: the DAL helper `_gd.projekt.GetByID` carries
/// `check_pr_id`; `role` for the role/permission families.
fn guard_level_for(checks: &str) -> Option<String> {
    if checks.is_empty() {
        return None;
    }
    static RE_OBJ_FAMILY: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)^(?:check_\w*id|checkaccess\w*|checkprojectaccess|hasaccessto\w*|isownerof\w*|canaccess\w*)$")
            .expect("RE_OBJ_FAMILY")
    });
    let object = checks
        .split(';')
        .map(|c| c.trim())
        .any(|c| !c.is_empty() && RE_OBJ_FAMILY.is_match(c));
    Some(if object {
        "object".into()
    } else {
        "role".into()
    })
}

/// Helper candidates a function may inherit a guard from, in preference
/// order: bare in-class calls found in the BODY (the VB extractor emits no
/// Calls edge for `CanUserBulkUpdate()`), then the graph's Calls targets
/// (qualified calls such as `_gd.projekt.GetByID`).
fn helper_candidates(
    graph: &std::sync::Arc<engram_graph::GraphStore>,
    pid: &str,
    n: &engram_graph::Node,
    body: Option<&str>,
    file_fns: &HashMap<(String, String), engram_graph::Node>,
    failures: &mut Vec<String>,
) -> Vec<engram_graph::Node> {
    static RE_BARE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?:^|[^.\w])([A-Za-z_]\w*)\s*\(").expect("RE_BARE")
    });
    let mut out: Vec<engram_graph::Node> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let file = n.file_path.as_str().replace('\\', "/");
    if let Some(b) = body {
        for line in b.lines() {
            let code = line.split('\'').next().unwrap_or(line);
            for cap in RE_BARE.captures_iter(code) {
                let key = (file.clone(), cap[1].to_ascii_lowercase());
                if let Some(t) = file_fns.get(&key)
                    && t.node_id != n.node_id
                    && seen.insert(t.node_id.clone())
                {
                    out.push(t.clone());
                }
            }
        }
    }
    match graph.neighbors(pid, EdgeKind::Calls, &n.node_id, GUARD_HELPER_HOP_CAP) {
        Ok(neigh) => {
            for (target, _) in neigh {
                if seen.contains(&target) {
                    continue;
                }
                match graph.get_node(pid, &target) {
                    Ok(Some(t)) => {
                        seen.insert(target);
                        out.push(t);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        failures.push(format!("{}: helper lookup {target} failed: {e}", n.name))
                    }
                }
            }
        }
        Err(e) => failures.push(format!("{}: Calls lookup failed: {e}", n.name)),
    }
    out
}

/// Client-supplied scope keys read in a function body.
pub(crate) fn client_scope_reads(body: &str) -> Vec<String> {
    static RE_READS: std::sync::LazyLock<Vec<regex::Regex>> = std::sync::LazyLock::new(|| {
        [
            r#"(?i)qry\.(?:params|data)\s*\(\s*"(\w+)"\s*\)"#,
            r#"(?i)GetDictionary\w*Value\s*\(\s*qry\.(?:params|data)\s*,\s*"(\w+)"\s*\)"#,
            r#"(?i)\bRequest(?:\.QueryString|\.Form|\.Params)?\s*\(\s*"(\w+)"\s*\)"#,
        ]
        .iter()
        .map(|p| regex::Regex::new(p).expect("RE_READS"))
        .collect()
    });
    // Source order (first read first), whichever pattern matched it.
    let mut found: Vec<(usize, String)> = Vec::new();
    for re in RE_READS.iter() {
        for cap in re.captures_iter(body) {
            let k = cap[1].to_string();
            let at = cap.get(1).map(|m| m.start()).unwrap_or(0);
            if !found.iter().any(|(_, existing)| *existing == k) {
                found.push((at, k));
            }
        }
    }
    found.sort_by_key(|(at, _)| *at);
    found.into_iter().map(|(_, k)| k).collect()
}

/// True when every line of `body` that mentions one of `checks` (the
/// extractor's `permission_checks`, `;`-separated, lower-case) is indented
/// deeper than the function's top-level statements — i.e. the check runs
/// only on some branch. False when at least one check is a top-level guard
/// clause, or when no check line is found at all.
pub(crate) fn own_checks_all_conditional(body: &str, checks: &str) -> bool {
    let names: Vec<String> = checks
        .split(';')
        .map(|c| c.trim().to_ascii_lowercase())
        .filter(|c| !c.is_empty())
        .collect();
    if names.is_empty() {
        return false;
    }
    let lines: Vec<&str> = body.lines().collect();
    // Top-level statement indent: the smallest indent of a non-blank line
    // after the declaration line, excluding `End …` lines.
    let top = lines
        .iter()
        .skip(1)
        .filter(|l| {
            let t = l.trim();
            !t.is_empty() && !t.to_ascii_lowercase().starts_with("end ")
        })
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    let mut seen = false;
    for l in lines.iter().skip(1) {
        let lower = l.to_ascii_lowercase();
        if !names.iter().any(|n| lower.contains(n)) {
            continue;
        }
        seen = true;
        let indent = l.len() - l.trim_start().len();
        if indent <= top {
            return false; // a top-level guard clause
        }
    }
    seen
}

/// An OBJECT-level guard in the body: the check takes the scope value
/// (`check_pr_id(pr_id)`, `CheckAccessToProject(pr_id)`, …).
pub(crate) fn has_object_level_guard(body: &str) -> bool {
    static RE_OBJ: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"(?i)\b(?:check_pr_id|check_\w+_id|CheckAccess\w*|CheckProjectAccess|HasAccessTo\w*|IsOwnerOf\w*|CanAccess\w*)\s*\(",
        )
        .expect("RE_OBJ")
    });
    RE_OBJ.is_match(body)
}

pub(crate) fn build_guards_report(
    graph: &std::sync::Arc<engram_graph::GraphStore>,
    pid: &str,
    scope: Option<&str>,
    root: &std::path::Path,
) -> GuardsReport {
    let mut cov = GuardsCoverage {
        node_scan: "complete".into(),
        node_scan_cap: crate::handlers::NODE_SCAN_LIMIT,
        scope_query: "none".into(),
        caps: vec![
            format!(
                "project-wide node scan {}",
                crate::handlers::NODE_SCAN_LIMIT
            ),
            format!(
                "settings: functions {GUARD_SETTINGS_FN_CAP}, edges per function {GUARD_SETTINGS_EDGE_CAP}, tables {GUARD_SETTINGS_TABLE_CAP}"
            ),
            format!("helper hop: {GUARD_HELPER_HOP_CAP} calls per function"),
            format!("markdown lists {GUARD_LIST_CAP} per section (full lists in JSON)"),
            format!("house patterns {GUARD_HOUSE_CAP}, roles {GUARD_ROLES_CAP}"),
        ],
        ..Default::default()
    };
    let scope_lc = scope.map(|s| s.to_lowercase());

    // Project-wide scan (house patterns, settings tables, app settings).
    let all_nodes = match graph.query_nodes(pid, None, None, None, crate::handlers::NODE_SCAN_LIMIT)
    {
        Ok(n) => n,
        Err(e) => {
            cov.failures
                .push(format!("project-wide node scan failed: {e}"));
            Vec::new()
        }
    };
    cov.scanned = all_nodes.len();
    if all_nodes.len() >= crate::handlers::NODE_SCAN_LIMIT {
        cov.node_scan = "truncated".into();
    }

    // Scoped function set: a path-like scope is a STORE query.
    let mut scoped: Vec<engram_graph::Node> = Vec::new();
    if let Some(sc) = scope {
        let path_like = sc.contains('/') || sc.contains('.');
        if path_like {
            match graph.query_nodes(
                pid,
                Some("function"),
                None,
                Some(sc),
                crate::handlers::NODE_SCAN_LIMIT,
            ) {
                Ok(n) if !n.is_empty() => {
                    cov.scope_query = "store".into();
                    scoped = n;
                }
                Ok(_) => {}
                Err(e) => cov.failures.push(format!("scoped store query failed: {e}")),
            }
        }
        if scoped.is_empty() {
            cov.scope_query = "filter".into();
            let s = scope_lc.clone().unwrap_or_default();
            scoped = all_nodes
                .iter()
                .filter(|n| n.node_type == "function")
                .filter(|n| {
                    let fp = n.file_path.as_str().replace('\\', "/").to_lowercase();
                    fp.contains(&s) || n.name.to_lowercase() == s
                })
                .cloned()
                .collect();
        }
    } else {
        scoped = all_nodes
            .iter()
            .filter(|n| n.node_type == "function")
            .cloned()
            .collect();
    }
    scoped.sort_by(|a, b| {
        a.file_path
            .as_str()
            .cmp(b.file_path.as_str())
            .then(a.start_line.cmp(&b.start_line))
    });
    cov.in_scope_functions = scoped.len();

    let mut report = GuardsReport {
        scope: scope.map(|s| s.to_string()),
        app_settings_defined: 0,
        ..Default::default()
    };

    // House patterns from the project-wide scan.
    let mut house: HashMap<String, usize> = HashMap::new();
    let mut roles_seen: HashMap<String, usize> = HashMap::new();
    let mut settings_tables: Vec<(String, String)> = Vec::new();
    for n in &all_nodes {
        if n.node_type == "function" {
            let checks = node_meta_str(n, "permission_checks");
            if !checks.is_empty() {
                for g in checks.split(';').filter(|g| !g.is_empty()) {
                    *house.entry(g.to_string()).or_default() += 1;
                }
                for r in node_meta_str(n, "guard_roles")
                    .split(';')
                    .filter(|r| !r.is_empty())
                {
                    *roles_seen.entry(r.to_string()).or_default() += 1;
                }
            }
        } else if n.node_type == "app_setting" {
            report.app_settings_defined += 1;
        } else if n.node_type == "db_table" && is_settings_table_name(&n.name) {
            settings_tables.push((n.node_id.clone(), n.name.clone()));
        }
    }
    let mut hs: Vec<(String, usize)> = house.into_iter().collect();
    hs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    report.house_patterns = hs;
    let mut rs: Vec<(String, usize)> = roles_seen.into_iter().collect();
    rs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    report.roles_seen = rs;

    // Verdict per scoped function (A1 + A3), client-input rule (A2).
    let mut file_lines: HashMap<String, Option<Vec<String>>> = HashMap::new();
    // (file, bare lower-case name) → function node, for bare in-class calls.
    let file_fns: HashMap<(String, String), engram_graph::Node> = all_nodes
        .iter()
        .filter(|n| n.node_type == "function")
        .map(|n| {
            (
                (
                    n.file_path.as_str().replace('\\', "/"),
                    n.name
                        .rsplit('.')
                        .next()
                        .unwrap_or(&n.name)
                        .to_ascii_lowercase(),
                ),
                n.clone(),
            )
        })
        .collect();
    for n in &scoped {
        let checks = node_meta_str(n, "permission_checks").to_string();
        let roles = node_meta_str(n, "guard_roles").to_string();
        let fallback = node_meta_str(n, "extraction_fallback") == "true";
        let bare = n.name.rsplit('.').next().unwrap_or(&n.name).to_string();
        let file = n.file_path.as_str().replace('\\', "/");
        let body: Option<String> = {
            let lines = file_lines.entry(file.clone()).or_insert_with(|| {
                match std::fs::read_to_string(root.join(&file)) {
                    Ok(t) => Some(t.lines().map(|l| l.to_string()).collect::<Vec<_>>()),
                    Err(e) => {
                        cov.failures.push(format!(
                            "{file}: unreadable — client-input reads not analysed: {e}"
                        ));
                        None
                    }
                }
            });
            lines.as_ref().and_then(|ls| {
                let start = (n.start_line.max(1) - 1) as usize;
                let end = (n.end_line as usize).min(ls.len());
                (start < end).then(|| ls[start..end].join("\n"))
            })
        };
        let scope_reads = body.as_deref().map(client_scope_reads).unwrap_or_default();
        let object_level = body.as_deref().is_some_and(has_object_level_guard);
        let mut v = GuardVerdict {
            name: bare.clone(),
            file: file.clone(),
            line: n.start_line,
            verdict: String::new(),
            family: checks.clone(),
            level: if object_level {
                Some("object".into())
            } else {
                guard_level_for(&checks)
            },
            roles: roles.clone(),
            via: None,
            reason: String::new(),
            scope_reads,
            role_only: false,
            own_check_conditional: false,
        };
        let candidates =
            helper_candidates(graph, pid, n, body.as_deref(), &file_fns, &mut cov.failures);
        let credit = |v: &mut GuardVerdict, t: &engram_graph::Node, why: &str| {
            let tc = node_meta_str(t, "permission_checks");
            let tb = t.name.rsplit('.').next().unwrap_or(&t.name).to_string();
            v.verdict = "guarded".into();
            v.family = tc.to_string();
            v.level = guard_level_for(tc);
            v.roles = node_meta_str(t, "guard_roles").to_string();
            v.via = Some(tb.clone());
            v.reason = format!("{why} {tb} (one call hop)");
        };
        if !checks.is_empty() {
            v.verdict = "guarded".into();
            v.reason = "permission check in the function body (extractor metadata)".into();
            if body
                .as_deref()
                .is_some_and(|b| own_checks_all_conditional(b, &checks))
            {
                v.own_check_conditional = true;
                v.reason = "own permission check runs only on a branch".into();
                if let Some(t) = candidates
                    .iter()
                    .find(|t| !node_meta_str(t, "permission_checks").is_empty())
                {
                    credit(
                        &mut v,
                        t,
                        "own check runs only on a branch; guarded by helper",
                    );
                }
            }
            // A role-level own check plus a called helper that scopes the
            // OBJECT (`check_pr_id` inside the DAL) is object-level via that
            // helper — live: the bulk endpoints through `_gd.projekt.GetByID`.
            if v.level.as_deref() != Some("object") && !v.scope_reads.is_empty() {
                if let Some(t) = candidates.iter().find(|t| {
                    guard_level_for(node_meta_str(t, "permission_checks")).as_deref()
                        == Some("object")
                }) {
                    let tb = t.name.rsplit('.').next().unwrap_or(&t.name).to_string();
                    let tc = node_meta_str(t, "permission_checks").to_string();
                    v.level = Some("object".into());
                    // `via` names the credited GUARD helper when there is one
                    // (the conditional own check above); the scoping helper
                    // is named in the reason and becomes `via` only when no
                    // guard helper was credited.
                    if v.via.is_none() {
                        v.via = Some(tb.clone());
                    }
                    v.reason = format!("{} — object scoping by helper {tb} ({tc})", v.reason);
                }
            }
        } else if fallback {
            v.verdict = "unknown".into();
            v.reason =
                "symbol came from the extraction fallback — its guard metadata cannot be trusted"
                    .into();
        } else {
            if let Some(t) = candidates
                .iter()
                .find(|t| !node_meta_str(t, "permission_checks").is_empty())
            {
                credit(&mut v, t, "inherited from helper");
            }
            if v.verdict.is_empty() {
                v.verdict = "unguarded".into();
                v.reason =
                    "no permission check in the body and none in any directly called helper".into();
            }
        }
        if v.verdict == "guarded"
            && !v.scope_reads.is_empty()
            && v.level.as_deref() != Some("object")
        {
            v.role_only = true;
            v.reason = format!(
                "{} — reads client {} but no object-level guard covers it (ROLE-ONLY)",
                v.reason,
                v.scope_reads.join(", ")
            );
            report.role_only.push(bare.clone());
        }
        match v.verdict.as_str() {
            "guarded" => report.guarded.push(bare.clone()),
            "unknown" => report.unknown.push(bare.clone()),
            _ => report.unguarded.push(bare.clone()),
        }
        report.functions.push(v);
    }

    // Settings read by scoped functions (bounded, reported).
    let mut settings_read: HashMap<String, Vec<String>> = HashMap::new();
    for n in scoped.iter().take(GUARD_SETTINGS_FN_CAP) {
        match graph.neighbors(
            pid,
            EdgeKind::ReadsSetting,
            &n.node_id,
            GUARD_SETTINGS_EDGE_CAP,
        ) {
            Ok(neigh) => {
                for (target, _) in neigh {
                    let key = if let Some(rest) = target.strip_prefix("::") {
                        format!("{rest} (not in web.config — DB/env setting?)")
                    } else {
                        match graph.get_node(pid, &target) {
                            Ok(Some(t)) => t.name,
                            Ok(None) => target.clone(),
                            Err(e) => {
                                cov.failures
                                    .push(format!("setting node {target} lookup failed: {e}"));
                                target.clone()
                            }
                        }
                    };
                    settings_read.entry(key).or_default().push(n.name.clone());
                }
            }
            Err(e) => cov
                .failures
                .push(format!("{}: ReadsSetting lookup failed: {e}", n.name)),
        }
    }
    if scoped.len() > GUARD_SETTINGS_FN_CAP {
        cov.caps.push(format!(
            "settings read: only the first {GUARD_SETTINGS_FN_CAP} of {} in-scope functions were expanded",
            scoped.len()
        ));
    }
    let mut sr: Vec<(String, Vec<String>)> = settings_read
        .into_iter()
        .map(|(k, mut v)| {
            v.sort();
            v.dedup();
            (k, v)
        })
        .collect();
    sr.sort_by(|a, b| a.0.cmp(&b.0));
    report.settings_read = sr;

    if settings_tables.len() > GUARD_SETTINGS_TABLE_CAP {
        cov.caps.push(format!(
            "settings tables: {} found, consumers counted for the first {GUARD_SETTINGS_TABLE_CAP}",
            settings_tables.len()
        ));
    }
    for (table_id, table_name) in settings_tables.iter().take(GUARD_SETTINGS_TABLE_CAP) {
        match graph.find_incoming_edges_with_kind(pid, None, table_id, 500) {
            Ok(v) => {
                let count = v
                    .into_iter()
                    .filter(|(_, k, _)| {
                        matches!(
                            k,
                            EdgeKind::QueriesTable
                                | EdgeKind::SqlCalls
                                | EdgeKind::ReadsColumn
                                | EdgeKind::StoredProcReadsTable
                                | EdgeKind::StoredProcWritesTable
                        )
                    })
                    .count();
                report.settings_tables.push((table_name.clone(), count));
            }
            Err(e) => cov.failures.push(format!(
                "consumers of settings table {table_name} failed: {e}"
            )),
        }
    }
    report.coverage = cov;
    report
}

fn push_list(out: &mut String, items: &[String], render: impl Fn(&String) -> String) {
    for it in items.iter().take(GUARD_LIST_CAP) {
        out.push_str(&render(it));
    }
    if items.len() > GUARD_LIST_CAP {
        out.push_str(&format!(
            "  … and {} more (full list: output_json=true)\n",
            items.len() - GUARD_LIST_CAP
        ));
    }
}

pub(crate) fn render_guards_markdown(r: &GuardsReport) -> String {
    let mut out = format!(
        "# Guards & settings{}\n",
        r.scope
            .as_deref()
            .map(|s| format!(" — scope: {s}"))
            .unwrap_or_else(|| " — project-wide".into())
    );
    let total = r.functions.len();
    out.push_str(&format!(
        "\n## Guard parity\n{} guarded · {} UNGUARDED · {} UNKNOWN of {} function(s) in scope.\n",
        r.guarded.len(),
        r.unguarded.len(),
        r.unknown.len(),
        total
    ));
    out.push_str(&format!(
        "Level: {} object-level, {} ROLE-ONLY (a role check that does not cover the client scope key the \
         function reads), {} role-level without client scope reads.\n",
        r.functions.iter().filter(|f| f.level.as_deref() == Some("object")).count(),
        r.role_only.len(),
        r.functions
            .iter()
            .filter(|f| f.verdict == "guarded" && !f.role_only && f.level.as_deref() != Some("object"))
            .count()
    ));
    if !r.role_only.is_empty() {
        out.push_str("\n## ROLE-ONLY — client scope keys read without an object-level guard\n");
        let items: Vec<String> = r
            .functions
            .iter()
            .filter(|f| f.role_only)
            .map(|f| {
                format!(
                    "- ROLE-ONLY: {} ({}:{}) reads client {} — guard: {}\n",
                    f.name,
                    f.file,
                    f.line,
                    f.scope_reads.join(", "),
                    f.family
                )
            })
            .collect();
        push_list(&mut out, &items, |s| s.clone());
    }
    if !r.unguarded.is_empty() {
        out.push_str("\n## Unguarded functions in scope (verify each is intentionally public)\n");
        let items: Vec<String> = r
            .functions
            .iter()
            .filter(|f| f.verdict == "unguarded")
            .map(|f| format!("- UNGUARDED: {} ({}:{})\n", f.name, f.file, f.line))
            .collect();
        push_list(&mut out, &items, |s| s.clone());
    }
    if !r.unknown.is_empty() {
        out.push_str("\n## UNKNOWN — guard status cannot be trusted\n");
        let items: Vec<String> = r
            .functions
            .iter()
            .filter(|f| f.verdict == "unknown")
            .map(|f| format!("- {} ({}:{}) — {}\n", f.name, f.file, f.line, f.reason))
            .collect();
        push_list(&mut out, &items, |s| s.clone());
    }
    if !r.guarded.is_empty() {
        out.push_str("\n## Guarded functions in scope\n");
        let items: Vec<String> = r
            .functions
            .iter()
            .filter(|f| f.verdict == "guarded")
            .map(|f| {
                let role_str = if f.roles.is_empty() {
                    String::new()
                } else {
                    format!(" roles=[{}]", f.roles)
                };
                let via = f
                    .via
                    .as_deref()
                    .map(|v| format!(" via {v}"))
                    .unwrap_or_default();
                let via = if f.own_check_conditional {
                    format!("{via} (own check is branch-only)")
                } else {
                    via
                };
                format!(
                    "- {} ({}:{}) checks: {}{}{} [{}]\n",
                    f.name,
                    f.file,
                    f.line,
                    f.family,
                    via,
                    role_str,
                    f.level.as_deref().unwrap_or("?")
                )
            })
            .collect();
        push_list(&mut out, &items, |s| s.clone());
    }
    if !r.settings_read.is_empty() {
        out.push_str("\n## Settings read in scope\n");
        let items: Vec<String> = r
            .settings_read
            .iter()
            .map(|(k, fns)| format!("- {k} <- read by {}\n", fns.join(", ")))
            .collect();
        push_list(&mut out, &items, |s| s.clone());
    }
    if !r.settings_tables.is_empty() {
        out.push_str("\n## Settings-shaped tables (config stored in the DB)\n");
        for (table, count) in &r.settings_tables {
            out.push_str(&format!(
                "- {table} — {count} code/SP consumer edge(s); changes to settings semantics ripple here\n"
            ));
        }
    }
    if r.house_patterns.is_empty() {
        out.push_str(
            "\n## House auth patterns\nNo guard calls detected anywhere — either the project predates \
             this extraction (re-run update_project) or authorization is enforced purely via web.config \
             (see map_auth_config).\n",
        );
    } else {
        out.push_str("\n## House auth patterns (project-wide guard helpers)\n");
        for (g, c) in r.house_patterns.iter().take(GUARD_HOUSE_CAP) {
            out.push_str(&format!("- {g} ({c} function(s))\n"));
        }
        if r.house_patterns.len() > GUARD_HOUSE_CAP {
            out.push_str(&format!(
                "  … and {} more\n",
                r.house_patterns.len() - GUARD_HOUSE_CAP
            ));
        }
        if !r.roles_seen.is_empty() {
            let names: Vec<String> = r
                .roles_seen
                .iter()
                .take(GUARD_ROLES_CAP)
                .map(|(role, c)| format!("{role} ({c})"))
                .collect();
            out.push_str(&format!("roles referenced: {}\n", names.join(", ")));
        }
    }
    let c = &r.coverage;
    out.push_str("\n## Coverage\n");
    out.push_str(&format!(
        "- node scan: {} ({} nodes, cap {}) · scope query: {} · in-scope functions: {}\n",
        c.node_scan, c.scanned, c.node_scan_cap, c.scope_query, c.in_scope_functions
    ));
    for cap in &c.caps {
        out.push_str(&format!("- cap: {cap}\n"));
    }
    if c.failures.is_empty() {
        out.push_str("- failures: none\n");
    } else {
        for f in &c.failures {
            out.push_str(&format!("- FAILURE: {f}\n"));
        }
    }
    out.push_str(&format!(
        "\napp settings defined in config files: {}\n\
         next: map_auth_config for web.config authorization rules; get_table_schema for each \
         settings table; trace_state_usage for role/session keys.\n",
        r.app_settings_defined
    ));
    out
}

#[cfg(test)]
mod guards_unit_tests {
    use super::*;

    #[test]
    fn markdown_lists_are_cut_with_a_stated_remainder() {
        let items: Vec<String> = (0..30).map(|i| format!("- f{i}\n")).collect();
        let mut out = String::new();
        push_list(&mut out, &items, |s| s.clone());
        assert!(out.contains("- f24\n"));
        assert!(!out.contains("- f25\n"));
        assert!(out.contains("… and 5 more"));
    }

    #[test]
    fn conditional_own_checks_are_detected_by_indent() {
        let cond = "Public Function F() As String\n    If isUpdatingAR Then\n        If Not _us.UserAccess.CheckWrite(x) Then Return s\n    End If\n    Return \"ok\"\nEnd Function\n";
        assert!(own_checks_all_conditional(cond, "checkwrite"));
        let guard = "Public Function F() As String\n    If Not _us.UserAccess.CheckWrite(x) Then Return s\n    Return \"ok\"\nEnd Function\n";
        assert!(!own_checks_all_conditional(guard, "checkwrite"));
        assert!(!own_checks_all_conditional(cond, ""));
    }

    #[test]
    fn client_scope_reads_and_object_guards() {
        let body = "Dim pr_id = GetDictionaryIntegerValue(qry.params, \"pr_id\")\nDim x = Request(\"id\")\n";
        assert_eq!(
            client_scope_reads(body),
            vec!["pr_id".to_string(), "id".to_string()]
        );
        assert!(!has_object_level_guard(body));
        // POST body (live: the bulk endpoints) — same rule.
        let post = "Dim projectID = GetDictionaryIntegerValue(qry.data, \"pr_id\")\nDim t = qry.data(\"typeID\")\n";
        assert_eq!(
            client_scope_reads(post),
            vec!["pr_id".to_string(), "typeID".to_string()]
        );
        assert!(has_object_level_guard(
            "If Not _us.accessctrl.check_pr_id(pr_id) Then Return"
        ));
        assert!(has_object_level_guard(
            "If Not CheckAccessToProject(prId) Then"
        ));
        assert!(!has_object_level_guard(
            "If Not _us.UserAccess.CheckRead(x) Then"
        ));
    }

    #[test]
    fn role_level_only_when_a_check_exists() {
        assert_eq!(guard_level_for(""), None);
        assert_eq!(guard_level_for("CheckRead").as_deref(), Some("role"));
    }
}

pub(crate) fn is_settings_table_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    ["setting", "config", "option", "param", "preference"]
        .iter()
        .any(|p| lower.contains(p))
}

/// Does the story ask for ANALYTICS OVER TIME (time-to-X, aging, trends,
/// audit history)? Such stories need the domain entity's STATUS-CHANGE
/// HISTORY — which lives in log/history tables the concept/co-change arms
/// miss, because the story names the entity, not its log table.
pub(crate) fn story_asks_analytics_over_time(story: &str) -> bool {
    let s = story.to_lowercase();
    [
        "time to",
        "aging",
        "over time",
        "history",
        "audit",
        "trend",
        "duration",
        "elapsed",
        "performance of",
        "completion",
    ]
    .iter()
    .any(|t| s.contains(t))
}

/// Log/history/audit table-name shape (db_table node names are lowercase).
/// The "log" check is boundary-guarded so catalog/dialog/login/blog/logistics
/// don't match — the same false-positive class the token-boundary concept
/// matching in this file exists to prevent.
pub(crate) fn is_history_log_table_name(name: &str) -> bool {
    let n = name.to_lowercase();
    if n.contains("hist") || n.contains("audit") {
        return true;
    }
    n.match_indices("log").any(|(i, _)| {
        let pre = &n[..i];
        let post = &n[i + 3..];
        !(pre.ends_with("cata")
            || pre.ends_with("dia")
            || pre.ends_with('b')
            || post.starts_with("in")
            || post.starts_with("ist")
            || post.starts_with("ic"))
    })
}

/// Stopword filter for deterministic concept extraction from a user story.
/// Words that never name a domain concept in a user story.
const STORY_STOPWORDS: &[&str] = &[
    "as",
    "an",
    "a",
    "the",
    "i",
    "we",
    "to",
    "of",
    "in",
    "on",
    "for",
    "and",
    "or",
    "is",
    "are",
    "be",
    "able",
    "would",
    "like",
    "want",
    "need",
    "needs",
    "should",
    "must",
    "can",
    "could",
    "set",
    "sets",
    "add",
    "adds",
    "new",
    "get",
    // Generic CRUD / UI verbs: almost never the DISTINCTIVE concept (the
    // domain noun or identifier is). Demoting them stops the narrative
    // preamble ("update invoice status") from crowding out the high-signal
    // token the story actually names (e.g. the API resource `roqentries`).
    // Keep DOMAIN nouns/features (report, export, import, search, filter,
    // map, invoice, status, …) as real concepts — only pure verbs here.
    // NOTE: only UNAMBIGUOUS generic verbs. Deliberately NOT stopwording
    // verbs that double as domain nouns in the pilot corpus — change ("Change Requests"),
    // view (DB/map views), select (SQL), process (business process), create
    // (creation flows) — to avoid hurting recall on those concepts.
    "update",
    "updates",
    "modify",
    "edit",
    "edits",
    "include",
    "includes",
    "included",
    "including",
    "display",
    "show",
    "shows",
    "manage",
    "enable",
    "enabled",
    "disable",
    "disabled",
    "toggle",
    "choose",
    "save",
    "remove",
    "delete",
    "handle",
    "avoid",
    "prevent",
    // Story-structure / Gherkin labels and HTTP verbs: scaffolding, not
    // domain concepts (they otherwise steal a top-3 slot from real tokens).
    "acceptance",
    "criteria",
    "scenario",
    "given",
    "post",
    "patch",
    "call",
    "calls",
    "have",
    "has",
    "when",
    "with",
    "that",
    "this",
    "it",
    "my",
    "our",
    "so",
    "user",
    "users",
    "admin",
    "admins",
    "administrator",
    "system",
    "page",
    "allow",
    "allows",
    "make",
    "required",
    "require",
    "minimum",
    "maximum",
    "number",
    "amount",
    "count",
    "via",
    "from",
    "into",
    "their",
    "them",
    "they",
    "if",
    "then",
    "also",
    "story",
];

/// The acceptable-token filter behind story concept extraction: lowercase,
/// >= 4 chars, not a stopword, not hash/ID garbage.
pub(crate) fn story_token(w: &str) -> Option<String> {
    let lower = w.to_lowercase();
    if lower.len() < 4 || STORY_STOPWORDS.contains(&lower.as_str()) {
        return None;
    }
    // Reject hash/ID garbage that leaks into PR-derived stories: commit SHAs
    // ("a778c06a"), usernames/board IDs, ticket numbers. A real
    // domain concept almost never carries 3+ digits; such tokens otherwise
    // steal the limited concept slots and tank recall. Generic, no per-repo
    // names — robustness to noisy story input.
    if lower.chars().filter(char::is_ascii_digit).count() >= 3 {
        return None;
    }
    Some(lower)
}

/// Story concept CANDIDATES (row-1 audit A1). The plain document-order
/// recipe comes first and is never dropped (it is what the eval validated);
/// then the author's own domain names: parenthesized glosses ("… category
/// (huvudredovisningskategori)") and adjacent non-stopword pairs/triples
/// (noun phrases). [`resolve_story_concepts`] decides which of the extras
/// survive by asking the index.
/// External audit 2026-08-29 P0-3: the parenthesized glosses of a story —
/// "a main reporting category (huvudredovisningskategori)" — are the author
/// naming the entity in the code's own language. They are the one class of
/// candidate that retrieves BY DEFAULT (index-corroborated, compound suffix
/// split by `resolve_story_concepts`); noun-phrase expansions stay opt-in
/// because they inflated the weak tier on the 5-PR gate (03 §7).
pub(crate) fn extract_story_gloss_concepts(story: &str) -> Vec<String> {
    let re_paren = regex::Regex::new(r"\(([^()]{4,60})\)").expect("paren regex");
    let mut out: Vec<String> = Vec::new();
    for cap in re_paren.captures_iter(story) {
        let inner = cap[1].trim().to_lowercase();
        let plain = inner
            .chars()
            .all(|c| c.is_alphanumeric() || c == ' ' || c == '-' || c == '_');
        let digits = inner.chars().filter(char::is_ascii_digit).count();
        if plain && digits < 3 && !out.contains(&inner) {
            out.push(inner);
        }
    }
    out
}

/// Which resolved candidates came from a gloss: the gloss itself, its
/// compacted form, or a compound suffix of it (`huvudredovisningskategori`
/// → `redovisningskategori`).
pub(crate) fn gloss_derived<'a>(glosses: &[String], candidates: &'a [String]) -> Vec<&'a String> {
    candidates
        .iter()
        .filter(|c| {
            glosses.iter().any(|g| {
                let compact: String = g.chars().filter(|ch| !ch.is_whitespace()).collect();
                g == *c || compact == **c || (c.len() >= 8 && compact.ends_with(c.as_str()))
            })
        })
        .collect()
}

pub(crate) fn extract_story_concept_candidates(story: &str) -> Vec<String> {
    let mut out = extract_story_concepts(story);
    let mut seen: HashSet<String> = out.iter().cloned().collect();

    // Parenthesized glosses: a story author naming the entity in the
    // code's own language (or a synonym) is the strongest cue there is.
    let re_paren = regex::Regex::new(r"\(([^()]{4,60})\)").expect("paren regex");
    for cap in re_paren.captures_iter(story) {
        let inner = cap[1].trim().to_lowercase();
        let plain = inner
            .chars()
            .all(|c| c.is_alphanumeric() || c == ' ' || c == '-' || c == '_');
        let digits = inner.chars().filter(char::is_ascii_digit).count();
        if plain && digits < 3 && seen.insert(inner.clone()) {
            out.push(inner);
        }
    }

    // Noun phrases: runs of acceptable tokens, longest window first.
    let toks: Vec<Option<String>> = story
        .split(|c: char| !c.is_alphanumeric())
        .map(story_token)
        .collect();
    for win in [3usize, 2] {
        if toks.len() < win {
            continue;
        }
        for i in 0..=toks.len() - win {
            let slice = &toks[i..i + win];
            if slice.iter().all(Option::is_some) {
                let phrase = slice
                    .iter()
                    .map(|t| t.clone().unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join(" ");
                if seen.insert(phrase.clone()) {
                    out.push(phrase);
                }
            }
        }
    }
    out.truncate(24);
    out
}

/// Keep the first three candidates unconditionally (the recipe), then
/// append extras the INDEX corroborates, up to `max`:
/// - a phrase whose compact form (spaces removed) occurs in some indexed
///   path;
/// - a single token >= 5 chars that occurs in some indexed path;
/// - a long single token (>= 10 chars) that does NOT occur as-is but whose
///   SUFFIX (>= 8 chars) does — a compound split ("huvudredovisningskategori"
///   -> "redovisningskategori"), which is how the story's language reaches
///   the code's.
/// Uncorroborated extras are dropped; with an empty index the result is the
/// plain recipe.
pub(crate) fn resolve_story_concepts(
    cands: &[String],
    index: &[String],
    max: usize,
) -> Vec<String> {
    let paths: Vec<String> = index
        .iter()
        .map(|p| p.replace('\\', "/").to_lowercase())
        .collect();
    let occurs = |needle: &str| -> bool { paths.iter().any(|p| p.contains(needle)) };
    let mut out: Vec<String> = cands.iter().take(3).cloned().collect();
    let mut seen: HashSet<String> = out.iter().cloned().collect();
    for c in cands.iter().skip(3) {
        if out.len() >= max {
            break;
        }
        let c = c.to_lowercase();
        if seen.contains(&c) {
            continue;
        }
        if c.contains(' ') {
            let compact: String = c.chars().filter(|ch| !ch.is_whitespace()).collect();
            if compact.len() >= 6 && occurs(&compact) && seen.insert(c.clone()) {
                out.push(c);
            }
            continue;
        }
        if c.len() >= 5 && occurs(&c) {
            if seen.insert(c.clone()) {
                out.push(c);
            }
            continue;
        }
        if c.chars().count() >= 10 {
            // Compound split: the longest corroborated suffix wins.
            let chars: Vec<char> = c.chars().collect();
            for k in 1..=chars.len().saturating_sub(8) {
                let suffix: String = chars[k..].iter().collect();
                if occurs(&suffix) {
                    if seen.insert(suffix.clone()) {
                        out.push(suffix);
                    }
                    break;
                }
            }
        }
    }
    out
}

pub(crate) fn extract_story_concepts(story: &str) -> Vec<String> {
    let candidate = story_token;
    let mut seen = HashSet::new();
    let mut out: Vec<String> = Vec::new();

    // 1. High-signal first: tokens the story puts in a resource/endpoint PATH
    //    (e.g. `api/v2/roqentries/{id}/setasbilled`) name a concrete code
    //    resource and are almost always the exact change target — but they sit
    //    deep in the description, past the narrative preamble, where a plain
    //    document-order scan (top-3) never reaches them. Stories without such
    //    paths skip this loop entirely (near-zero regression). Cap at 2 so
    //    ordinary prose concepts still get a slot.
    let path_re = regex::Regex::new(r"[/\\]([A-Za-z][A-Za-z0-9_]{3,})")
        .expect("extract_story_concepts path regex");
    for cap in path_re.captures_iter(story) {
        if out.len() >= 2 {
            break;
        }
        if let Some(c) = candidate(&cap[1])
            && seen.insert(c.clone())
        {
            out.push(c);
        }
    }

    // 2. Fill remaining slots in document order.
    for word in story.split(|c: char| !c.is_alphanumeric()) {
        if out.len() >= 3 {
            break;
        }
        if let Some(c) = candidate(word)
            && seen.insert(c.clone())
        {
            out.push(c);
        }
    }

    // 3. Auth/permission supplement (appended BEYOND the domain cap so it never
    //    displaces a domain concept). When a story gates the change on a role,
    //    permission or authorization — "restricted to the Administrator role",
    //    "requires X permission" — the auth/permission layer (the role/user model,
    //    authorize filters) is the file most often missed, because "admin"/
    //    "administrator" are stopwords and "role" sits past the top-3 cutoff. The
    //    concept `role` footprints to that layer (validated: surfaces the user/role
    //    model) in any ASP.NET app. Generic — no per-repo names.
    const AUTH_CUES: &[&str] = &[
        "role",
        "permission",
        "authoriz",
        "privilege",
        "access control",
        "rbac",
    ];
    let lower_story = story.to_lowercase();
    if AUTH_CUES.iter().any(|cue| lower_story.contains(cue)) {
        let auth = "role".to_string();
        if seen.insert(auth.clone()) {
            out.push(auth);
        }
    }
    out
}

impl Engram {
    pub async fn handle_map_guards_and_settings(
        &self,
        req: crate::models::MapGuardsAndSettingsRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let scope_raw = req.scope.as_deref().map(|s| s.replace('\\', "/"));
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let scope_b = scope_raw.clone();
        let project_dir = self
            .state
            .registry
            .get_project(&req.project_id)
            .ok()
            .flatten()
            .map(|r| r.directory)
            .unwrap_or_default();
        let report = tokio::task::spawn_blocking(move || {
            let root = std::path::PathBuf::from(project_dir);
            build_guards_report(&graph, &pid, scope_b.as_deref(), &root)
        })
        .await
        .map_err(|e| McpError::internal_error(format!("guards scan panicked: {e}"), None))?;
        if req.output_json {
            let text = serde_json::to_string_pretty(&report)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(text)]));
        }
        Ok(CallToolResult::success(vec![Content::text(
            render_guards_markdown(&report),
        )]))
    }

    /// One call: a weak user story in, an implementation brief out.
    pub async fn handle_plan_user_story(
        &self,
        req: crate::models::PlanUserStoryRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        if req.story.trim().is_empty() {
            return Err(McpError::invalid_params("story must not be empty", None));
        }
        let concepts: Vec<String> = match &req.concepts {
            Some(c) if !c.is_empty() => c.iter().take(3).cloned().collect(),
            _ => extract_story_concepts(&req.story),
        };

        let mut out = format!("# Implementation brief\n\nstory: {}\n", req.story.trim());
        out.push_str(&format!("concepts: {}\n", concepts.join(", ")));

        // Per-concept footprint (trimmed to keep the brief readable). One
        // failing concept must not kill the whole brief — degrade per-concept.
        for concept in &concepts {
            let sub = self
                .handle_get_concept_footprint(crate::models::GetConceptFootprintRequest {
                    project_id: req.project_id.clone(),
                    concept: concept.clone(),
                    max_per_group: 5,
                })
                .await;
            match sub {
                Ok(sub) => {
                    if let Some(text) = sub.content.first().and_then(|c| c.as_text()) {
                        let trimmed: String = text
                            .text
                            .lines()
                            .take_while(|l| !l.starts_with("next:") && !l.starts_with("---"))
                            .take(30)
                            .collect::<Vec<_>>()
                            .join("\n");
                        out.push_str(&format!("\n{trimmed}\n"));
                    }
                }
                Err(e) => {
                    out.push_str(&format!(
                        "\n(concept '{concept}': footprint unavailable — {})\n",
                        e.message
                    ));
                }
            }
        }

        // Pattern exemplars for the story's action.
        let sub = self
            .handle_find_implementation_pattern(crate::models::FindImplementationPatternRequest {
                project_id: req.project_id.clone(),
                pattern_query: concepts.join(" "),
                max_examples: 2,
                output_json: false,
            })
            .await?;
        if let Some(text) = sub.content.first().and_then(|c| c.as_text()) {
            let trimmed: String = text
                .text
                .lines()
                .take_while(|l| !l.starts_with("next:") && !l.starts_with("---"))
                .take(35)
                .collect::<Vec<_>>()
                .join("\n");
            out.push_str(&format!("\n{trimmed}\n"));
        }

        // Guards & settings overview (house patterns + settings tables).
        let sub = self
            .handle_map_guards_and_settings(crate::models::MapGuardsAndSettingsRequest {
                project_id: req.project_id.clone(),
                scope: None,
                output_json: false,
            })
            .await?;
        if let Some(text) = sub.content.first().and_then(|c| c.as_text()) {
            let mut lines: Vec<&str> = Vec::new();
            let mut keep = false;
            for l in text.text.lines() {
                if l.starts_with("## House auth patterns")
                    || l.starts_with("## Settings-shaped tables")
                {
                    keep = true;
                }
                if l.starts_with("next:") || l.starts_with("---") {
                    keep = false;
                }
                if keep {
                    lines.push(l);
                }
                if lines.len() > 25 {
                    break;
                }
            }
            if !lines.is_empty() {
                out.push_str(&format!("\n{}\n", lines.join("\n")));
            }
        }

        out.push_str(STORY_CHECKLIST);
        out.push_str(
            "\nnext: get_change_set(story=<this story>) for the RANKED FILE LIST \
             (concept+history+co-change+vector fused) — this brief explains the \
             domain; that tool names the files.\n",
        );
        let gen_ = self
            .get_active_generation(&req.project_id)
            .await
            .unwrap_or(1);
        out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }
}

#[cfg(test)]
mod guards_settings_tests {
    use super::*;

    #[test]
    fn settings_table_names_match_generic_shapes() {
        assert!(is_settings_table_name("ss_systemsettings"));
        assert!(is_settings_table_name("EmailSettings"));
        assert!(is_settings_table_name("app_config"));
        assert!(is_settings_table_name("UserOptions"));
        assert!(!is_settings_table_name("Orders"));
        assert!(!is_settings_table_name("Photos"));
    }

    #[test]
    fn history_log_table_names_match_and_reject_lookalikes() {
        for n in [
            "io_pr_iom_log",
            "order_status_history",
            "audit_trail",
            "pris_hist",
            "event_log",
            "log_entries",
        ] {
            assert!(is_history_log_table_name(n), "{n} should match");
        }
        for n in [
            "catalog",
            "dialog_settings",
            "user_login",
            "blog_posts",
            "logistics",
            "logic_rules",
            "orders",
        ] {
            assert!(!is_history_log_table_name(n), "{n} should NOT match");
        }
    }

    #[test]
    fn analytics_over_time_intent_detects_temporal_asks_only() {
        assert!(story_asks_analytics_over_time(
            "Report on TIME TO completion for purchase orders"
        ));
        assert!(story_asks_analytics_over_time(
            "Show order aging per customer"
        ));
        assert!(story_asks_analytics_over_time("visualize approval trends"));
        assert!(story_asks_analytics_over_time(
            "how long items stay in each status (duration)"
        ));
        assert!(!story_asks_analytics_over_time(
            "Add a new field to the customer form"
        ));
        assert!(!story_asks_analytics_over_time(
            "Rename the export button on the invoice page"
        ));
    }

    #[test]
    fn story_concepts_skip_stopwords_and_keep_domain_terms() {
        let c = extract_story_concepts(
            "As an admin I would like to set minimum number of photos required",
        );
        assert_eq!(c, vec!["photos".to_string()]);

        let c2 = extract_story_concepts("Allow adding users to a company via the public api");
        assert!(c2.contains(&"company".to_string()), "got {c2:?}");
        assert!(!c2.contains(&"users".to_string()), "users is generic");
    }
}

// ── generate_agent_integration ───────────────────────────────────────────────

/// Render the `.claude/rules/engram-workflow.md` content: the mandated loop.
/// "next:" footer of `get_concept_footprint` (agent-facing; schema-bound by
/// `agent_integration_tests`).
pub(crate) const NEXT_STEPS_CONCEPT_FOOTPRINT: &str = "\nnext: trace_state_usage for each state key; get_table_schema for each table; \
     detect_incomplete_changes(edited_files=...) for co-change partners (fast, precomputed). \
     If a name isn't found here the index may be behind — grep_project / read the \
     working tree before concluding it's absent.\n";

/// Closing checklist of `plan_user_story` (agent-facing; schema-bound by
/// `agent_integration_tests`).
pub(crate) const STORY_CHECKLIST: &str = "\n## Checklist (work through ALL of it — partial implementations are how \
     features ship without their admin page)\n\
     - [ ] Storage: does the new value/entity follow the house pattern above \
     (web.config key vs settings-table row)? Mirror the exemplar.\n\
     - [ ] Admin/config UI: where do users SET this? Find the page that manages \
     the sibling setting and extend it (or clone its pattern).\n\
     - [ ] Enforcement: apply the new rule at EVERY touchpoint listed in the \
     concept footprint above — uploads, edits, imports, APIs.\n\
     - [ ] Guards: match the house auth patterns — call map_guards_and_settings \
     with scope=<your service/page> and fix any UNGUARDED finding.\n\
     - [ ] Messages/UX: error/validation text wherever the rule can reject input.\n\
     - [ ] Then run: detect_incomplete_changes(edited_files=<your planned file list>) \
     (fast, precomputed co-change) and close every 'MISSING from your set' item.\n\
     - [ ] Per touched method: check_edit_safety. Before commit: pre_commit_review.\n";

/// `AGENTS.md` for agents that do not read `.claude/` (Codex, Copilot
/// agent mode, generic MCP clients): where Engram is, how to reach it, and
/// the same mandated workflow the Claude Code rules file carries.
pub(crate) fn render_agents_md(project_id: &str, directory: &str) -> String {
    let rules = render_workflow_rules(project_id, directory);
    // Drop the rules file's own H1; AGENTS.md supplies the top heading.
    let body = rules
        .split_once('\n')
        .map(|(_, rest)| rest.trim_start_matches('\n'))
        .unwrap_or(&rules);
    format!(
        "# AGENTS.md (generated by Engram `generate_agent_integration`)\n\n\
         This repository is indexed by **Engram**, an MCP server that holds the \
         project's code graph, git history, review memory and edit-safety \
         checks. It is exposed as the MCP server named `engram` (see the \
         `.mcp.json` snippet in the tool output if your client does not list \
         it). Claude Code reads the same rules from \
         `.claude/rules/engram-workflow.md`; this file is for every other agent.\n\n\
         {body}"
    )
}

/// Mergeable `.mcp.json` entry registering this very server binary as a
/// stdio MCP server. Emitted, never written: `.mcp.json` is usually
/// committed and the command is a machine-specific absolute path.
pub(crate) fn render_mcp_json_snippet(exe: &std::path::Path, config_path: Option<&str>) -> String {
    let mut engram = serde_json::json!({
        "type": "stdio",
        "command": exe.to_string_lossy(),
        "args": [],
    });
    if let Some(cfg) = config_path {
        engram["env"] = serde_json::json!({ "ENGRAM_CONFIG_PATH": cfg });
    }
    let v = serde_json::json!({ "mcpServers": { "engram": engram } });
    serde_json::to_string_pretty(&v).unwrap_or_default()
}

pub(crate) fn render_workflow_rules(project_id: &str, directory: &str) -> String {
    let gate_count = crate::services::pre_commit_review_service::gates::all_gates().len();
    format!(
        r#"# Engram workflow (generated by generate_agent_integration)

This project is indexed by Engram (project_id: `{project_id}`, directory
`{directory}`). These rules are MANDATORY — the tools exist because skipping
them is how regressions ship.

If Engram rejects that project_id (unknown project), the index was rebuilt
under a new id: call `list_projects` and use the entry whose directory is
`{directory}`, then re-run `generate_agent_integration(write_files=true)` so
this file stops lying.

## For every feature request / user story
1. `plan_user_story(story=<verbatim request>)` — concepts, footprints, house
   patterns, checklist. START HERE, even for "simple" stories.
2. `get_concept_footprint(concept=...)` for every domain concept you touch —
   change ALL touchpoints or justify each one you skip.
3. `find_implementation_pattern(pattern_query=...)` — imitate the house
   pattern; do not invent new approaches for solved problems.
4. `map_guards_and_settings(scope=<your file/service>)` — match the sibling
   guards and settings handling for ANY new endpoint or admin operation.

## Before and after editing
- `begin_edit_session(planned_files=[...])` BEFORE the first edit — the
  expectation brief names the couplings your plan must cover.
- Before modifying a method: `get_method_edit_context` or `check_edit_safety`.
- `complete_edit_session(edited_files=[...])` when done — scope drift +
  completeness in one call.
- After choosing your file set: `detect_incomplete_changes(edited_files=[...])` (fast,
  precomputed co-change) and close every item under "MISSING from your set".
  (`find_similar_changes` answers the same question but re-walks git history at
  call time — up to ~20s — so use it only as an optional deeper pass.)

## Before every commit
- `detect_incomplete_changes(edited_files=[...])` — history and state wiring
  name the files you forgot. Touch them or justify each one.
- `pre_commit_review(diff="staged")` — fix or explicitly justify every
  finding ({gate_count} gates, including guard_parity).
- `pre_push_audit(code=<your diff>, file_path=<file>)` — checks the change
  against the team's accumulated "what to avoid" knowledge (coding rules,
  copilot-instructions, CodeRabbit/SonarQube history, the recurring-issues
  board). Fix every rule it surfaces before pushing — these are mistakes the
  team has already flagged.
- If results ever look stale: `get_index_freshness`, then `update_project`.

## One-time project setup (so pre_push_audit has rules to check)
- For already-GENERIC sources, `ingest_quality_gates(source_path=..., source_type=...)`:
  `copilot` (copilot-instructions.md / coding-rules markdown) and `board` (DevOps
  recurring-issues export) — these are conventions, store them as-is.
- For raw FINDING corpora (a CodeRabbit or SonarQube export, often thousands of
  file/line-specific comments), use `distill_quality_gates(source_path=...,
  source_type=coderabbit|sonarqube)` instead. It clusters the findings and
  LLM-summarizes them into a small set of GENERIC rules ("the team keeps shipping
  un-parameterized SQL" -> "always parameterize") that apply to ANY change.
  Ingesting findings 1:1 is the wrong move — a finding tied to one file/line only
  ever helps someone editing that exact line; distillation is what makes the
  corpus reusable. Exclude/skip findings the team marked won't-fix.
- Re-run when a source is updated.
"#
    )
}

/// Render `.claude/settings.json` hook entries (Windows PowerShell or POSIX).
/// Hooks can't call MCP tools directly, so these are deterministic reminders:
/// their stdout is injected back into the conversation at exactly the moments
/// agents historically skip the workflow.
pub(crate) fn render_hooks_json(windows: bool) -> String {
    let (edit_cmd, stop_cmd) = if windows {
        (
            "powershell -NoProfile -Command \"Write-Output 'ENGRAM: source file changed. Before moving on: check_edit_safety for each touched method; map_guards_and_settings(scope=<file>) if you added/changed an endpoint. Before commit: detect_incomplete_changes(edited_files) + pre_commit_review(staged).'\"",
            "powershell -NoProfile -Command \"if (git status --porcelain 2>$null) { Write-Output 'ENGRAM: uncommitted changes present. Run detect_incomplete_changes(edited_files=<changed files>) and pre_commit_review(diff=staged) before finishing.' }\"",
        )
    } else {
        (
            "echo 'ENGRAM: source file changed. Before moving on: check_edit_safety for each touched method; map_guards_and_settings(scope=<file>) if you added/changed an endpoint. Before commit: detect_incomplete_changes(edited_files) + pre_commit_review(staged).'",
            "sh -c 'if [ -n \"$(git status --porcelain 2>/dev/null)\" ]; then echo \"ENGRAM: uncommitted changes present. Run detect_incomplete_changes(edited_files=<changed files>) and pre_commit_review(diff=staged) before finishing.\"; fi'",
        )
    };
    let v = serde_json::json!({
        "hooks": {
            "PostToolUse": [{
                "matcher": "Edit|Write|NotebookEdit",
                "hooks": [{ "type": "command", "command": edit_cmd }]
            }],
            "Stop": [{
                "matcher": "",
                "hooks": [{ "type": "command", "command": stop_cmd }]
            }]
        }
    });
    serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".to_string())
}

impl Engram {
    /// Emit (and optionally install) the Claude Code integration pack:
    /// workflow rules + reminder hooks that fire at the moments agents
    /// historically skip the Engram loop.
    pub async fn handle_generate_agent_integration(
        &self,
        req: crate::models::GenerateAgentIntegrationRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let rec = self.ensure_project_record(&req.project_id).await?;

        let rules = render_workflow_rules(&req.project_id, &rec.directory);
        let hooks = render_hooks_json(req.windows);
        let agents = render_agents_md(&req.project_id, &rec.directory);
        let exe = std::env::current_exe().unwrap_or_else(|_| "engram_server".into());
        let cfg_path = std::env::var("ENGRAM_CONFIG_PATH").ok();
        let mcp = render_mcp_json_snippet(&exe, cfg_path.as_deref());

        let mut out = String::with_capacity(4096);
        if req.write_files {
            let root = std::path::PathBuf::from(&rec.directory);
            let rules_rel = ".claude/rules/engram-workflow.md";
            let hooks_rel = ".claude/settings.json";
            let rules_path = engram_core::safe_join(&root, rules_rel)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            if let Some(dir) = rules_path.parent() {
                std::fs::create_dir_all(dir)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            }
            std::fs::write(&rules_path, &rules)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            out.push_str(&format!("wrote: {}\n", rules_path.display()));

            let hooks_path = engram_core::safe_join(&root, hooks_rel)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            if hooks_path.exists() {
                // Never clobber an existing settings.json — hooks must be
                // merged by a human (or the agent) deliberately.
                out.push_str(&format!(
                    "SKIPPED {} (exists) — merge the hooks block below manually.\n",
                    hooks_path.display()
                ));
                out.push_str(&format!(
                    "\n## hooks block to merge\n```json\n{hooks}\n```\n"
                ));
            } else {
                if let Some(dir) = hooks_path.parent() {
                    std::fs::create_dir_all(dir)
                        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                }
                std::fs::write(&hooks_path, &hooks)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                out.push_str(&format!("wrote: {}\n", hooks_path.display()));
            }

            let agents_path = engram_core::safe_join(&root, "AGENTS.md")
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            if agents_path.exists() {
                // AGENTS.md is commonly hand-authored; never clobber it.
                out.push_str(&format!(
                    "SKIPPED {} (exists) — add the Engram section below to it manually.\n",
                    agents_path.display()
                ));
                out.push_str(&format!(
                    "\n## AGENTS.md section to merge\n```markdown\n{agents}\n```\n"
                ));
            } else {
                std::fs::write(&agents_path, &agents)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                out.push_str(&format!("wrote: {}\n", agents_path.display()));
            }
        } else {
            out.push_str(&format!(
                "# Engram agent integration pack (write_files=false — contents below)\n\n\
                 ## .claude/rules/engram-workflow.md\n```markdown\n{rules}\n```\n\n\
                 ## .claude/settings.json (merge if one exists)\n```json\n{hooks}\n```\n\n\
                 ## AGENTS.md (for agents that do not read .claude/)\n```markdown\n{agents}\n```\n"
            ));
        }
        out.push_str(&format!(
            "\n## .mcp.json entry (NOT written — merge by hand; the command is machine-specific)\n\
             Registers this Engram binary for clients that read project-level MCP config \
             (Claude Code, Codex, VS Code/Visual Studio use `servers` instead of `mcpServers`).\n\
             ```json\n{mcp}\n```\n"
        ));
        out.push_str(
            "\nNOTE: hooks are deterministic REMINDERS (their output is injected back \
             into the agent's context at edit/stop time). Hard blocking requires an \
             Engram CLI entry point — tracked in TODO.md.\n",
        );
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }
}

// ── ingest_review_findings ───────────────────────────────────────────────────

/// Normalize a SonarQube /api/issues/search export into findings.
/// Component strings are "projectKey:path/to/file.ext".
pub(crate) fn parse_sonarqube_issues(json: &str) -> Vec<crate::models::ReviewFindingIn> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(issues) = v.get("issues").and_then(|i| i.as_array()) else {
        return Vec::new();
    };
    issues
        .iter()
        .filter_map(|i| {
            let message = i.get("message")?.as_str()?.to_string();
            let file = i
                .get("component")
                .and_then(|c| c.as_str())
                .and_then(|c| c.split_once(':').map(|(_, p)| p.to_string()));
            Some(crate::models::ReviewFindingIn {
                file,
                rule: i.get("rule").and_then(|r| r.as_str()).map(String::from),
                message,
                severity: i.get("severity").and_then(|s| s.as_str()).map(String::from),
            })
        })
        .collect()
}

impl Engram {
    /// Feed external review findings (CTO comments, SonarQube issues) into
    /// the anti-pattern index — what a reviewer caught once becomes something
    /// immune_check and pre_commit_review catch automatically forever.
    pub async fn handle_ingest_review_findings(
        &self,
        req: crate::models::IngestReviewFindingsRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;

        let mut findings: Vec<crate::models::ReviewFindingIn> =
            req.findings.clone().unwrap_or_default();
        let mut sq_count = 0usize;
        if let Some(ref json) = req.sonarqube_json {
            let sq = parse_sonarqube_issues(json);
            sq_count = sq.len();
            findings.extend(sq);
        }
        if findings.is_empty() {
            return Err(McpError::invalid_params(
                "no findings provided — pass `findings` and/or `sonarqube_json` \
                 (the {\"issues\":[...]} export from /api/issues/search)",
                None,
            ));
        }

        // Build anti-pattern docs (same namespace immune_check and the
        // pre-commit antipattern gate search).
        let mut docs = Vec::with_capacity(findings.len());
        let now = crate::utils::now_ms();
        for f in &findings {
            let source = if f.rule.as_deref().unwrap_or("").contains(':') {
                "sonarqube"
            } else {
                "manual_review"
            };
            let content = format!(
                "REVIEW FINDING\nSource: {source}\nRule: {}\nFile: {}\nSeverity: {}\n\n{}",
                f.rule.as_deref().unwrap_or("unspecified"),
                f.file.as_deref().unwrap_or("project-wide"),
                f.severity.as_deref().unwrap_or("unspecified"),
                f.message
            );
            let ch = engram_core::ContentHash::compute(content.as_bytes());
            let rel = f
                .file
                .clone()
                .unwrap_or_else(|| "review/manual".to_string());
            let doc_id = engram_core::DocIdStr::compute(&rel, 0, 0, &ch).0;
            docs.push(engram_index::IndexDoc {
                generation: gen_,
                chunk_id: engram_index::chunk_id_from_content_hash(&ch),
                path: engram_core::RelPath::new(&rel),
                language: "text".into(),
                content,
                namespace: "antipattern".into(),
                author: Some(source.to_string()),
                timestamp: Some(now / 1000),
                start_line: 0,
                end_line: 0,
                doc_id,
                content_hash: ch.0,
            });
        }
        let doc_count = docs.len();
        ps.search
            .index_docs(
                &req.project_id,
                &docs,
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // Promote severe, file-scoped findings to repo rules so they're
        // injected into get_chunk and checked by the immune gate.
        let mut promoted = 0usize;
        if req.promote_rules {
            let reg = self.state.registry.clone();
            let pid = req.project_id.clone();
            let candidates: Vec<(String, String)> = findings
                .iter()
                .filter(|f| {
                    f.file.is_some()
                        && matches!(
                            f.severity.as_deref().map(|s| s.to_lowercase()).as_deref(),
                            Some("blocker") | Some("critical") | Some("high")
                        )
                })
                .map(|f| {
                    let text: String = f.message.chars().take(300).collect();
                    (f.file.clone().unwrap_or_default(), text)
                })
                .collect();
            promoted = candidates.len();
            if !candidates.is_empty() {
                tokio::task::spawn_blocking(move || {
                    for (file, text) in candidates {
                        let hash =
                            engram_core::ContentHash::compute(format!("{file}:{text}").as_bytes());
                        let rule = engram_core::registry::RepoRule {
                            rule_id: format!("review_{}", &hash.0[..12]),
                            file_pattern: file,
                            rule_text: text,
                            priority: 2,
                            updated_at_ms: now,
                        };
                        let _ = reg.put_repo_rule(&pid, &rule);
                    }
                })
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            }
        }

        let mut out = format!(
            "Ingested {doc_count} review finding(s) into the anti-pattern index \
             ({sq_count} from SonarQube), {promoted} promoted to repo rules.\n\
             They are now checked by: immune_check (scoring), pre_commit_review's \
             antipattern + immune gates, and injected into get_chunk for the \
             affected files.\n"
        );
        out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }
}

#[cfg(test)]
mod review_ingest_tests {
    use super::*;

    #[test]
    fn sonarqube_issue_export_parses_to_findings() {
        let json = r#"{"issues":[
            {"component":"app:Site/App_Code/dal/users.vb","rule":"vbnet:S2077",
             "message":"Make sure using a dynamically formatted SQL query is safe here.",
             "severity":"CRITICAL","line":42},
            {"component":"app:Site/Default.aspx.vb","rule":"vbnet:S1481",
             "message":"Remove the unused local variable 'x'.","severity":"MINOR"}
        ]}"#;
        let findings = parse_sonarqube_issues(json);
        assert_eq!(findings.len(), 2);
        assert_eq!(
            findings[0].file.as_deref(),
            Some("Site/App_Code/dal/users.vb")
        );
        assert_eq!(findings[0].severity.as_deref(), Some("CRITICAL"));
        assert!(findings[0].message.contains("dynamically formatted SQL"));
    }

    #[test]
    fn malformed_sonarqube_json_yields_empty_not_panic() {
        assert!(parse_sonarqube_issues("not json").is_empty());
        assert!(parse_sonarqube_issues("{\"foo\":1}").is_empty());
    }
}

/// Extract repo-relative source-file paths from a planning tool's text output
/// (prose paths AND `kind:PATH:name:line` node-ids). Compound .NET code-behind
/// extensions are matched first so `foo.aspx.vb` is not truncated to `foo.aspx`.
fn change_set_paths(text: &str) -> Vec<String> {
    // GREEDY match (`*`, not `*?`): consume the whole path-char run, then
    // backtrack to the LAST valid extension. Lazy matching stopped at the first
    // extension-like token — e.g. a build-output directory named `~.js/` looks
    // like a `.js` file, so `modules/map/~.js/map.js` truncated to the dir.
    // The class also admits `~ @ +` (legal in build-output / scoped dirs) so
    // such paths aren't fragmented into slashless pieces the `keep` filter drops.
    let re = regex::Regex::new(
        r"(?i)[\w./\\~@+-]*\.(?:aspx\.vb|ascx\.vb|asax\.vb|asmx\.vb|ashx\.vb|svc\.vb|master\.vb|aspx\.cs|ascx\.cs|asax\.cs|asmx\.cs|ashx\.cs|svc\.cs|master\.cs|aspx|ascx|asax|ashx|asmx|svc|master|vb|css|cs|ts|tsx|js|jsx|sql|config|vbhtml|cshtml|resx|html|yaml|yml|dbml|edmx)\b",
    )
    .expect("change_set_paths regex");
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for m in re.find_iter(text) {
        let mut p = m.as_str().replace('\\', "/").to_lowercase();
        while p.contains("//") {
            p = p.replace("//", "/");
        }
        let p = p.trim_start_matches('/').to_string();
        if p.starts_with("http")
            || p.starts_with("c:")
            || p.starts_with("f:")
            || p.starts_with("d:")
        {
            continue;
        }
        let keep = p.contains('/') || p.ends_with(".config") || p.ends_with(".asax");
        if keep && seen.insert(p.clone()) {
            out.push(p);
        }
    }
    out
}

/// Architectural role suffix tokens shared across enterprise codebases. Used to
/// reduce a filename to its ENTITY stem so sibling files of one feature group
/// together (RoqEntriesController, RoqEntryService, IRoqEntryService, RoqEntry-In
/// all -> "roqentry"). Generic OO/enterprise vocabulary, not per-repo.
const SCAFFOLD_ROLE_SUFFIXES: &[&str] = &[
    "controller",
    "service",
    "repository",
    "manager",
    "provider",
    "factory",
    "handler",
    "validator",
    "mapper",
    "builder",
    "helper",
    "extensions",
    "job",
    "worker",
    "listener",
    "command",
    "query",
    "request",
    "response",
    "viewmodel",
    "model",
    "dto",
    "config",
];

/// Reduce a filename (no extension) to its entity stem: drop a leading interface
/// `I` (I + Uppercase), a trailing `-In`/`-Out` DTO marker, one trailing role
/// suffix, then singularize. Lowercased. Generic.
fn scaffold_stem(file_no_ext: &str) -> String {
    let mut s = file_no_ext;
    if s.len() > 2 && s.starts_with('I') && s.as_bytes().get(1).is_some_and(u8::is_ascii_uppercase)
    {
        s = &s[1..];
    }
    let mut lower = s.to_lowercase();
    for marker in ["-in", "-out", "_in", "_out"] {
        if let Some(p) = lower.strip_suffix(marker) {
            lower = p.to_string();
            break;
        }
    }
    for suf in SCAFFOLD_ROLE_SUFFIXES {
        if lower.len() > suf.len() + 2
            && let Some(p) = lower.strip_suffix(suf)
        {
            lower = p.to_string();
            break;
        }
    }
    let lower = lower.trim_end_matches(['-', '_', '.']);
    if let Some(p) = lower.strip_suffix("ies") {
        format!("{p}y")
    } else if lower.ends_with("ss") {
        lower.to_string()
    } else {
        lower.strip_suffix('s').unwrap_or(lower).to_string()
    }
}

/// Find the existing feature COHORT most useful as a scaffold template: an
/// (area, entity-stem) whose files span >=3 distinct directories AND (when a cue
/// token is given, e.g. "controller" for an API story) contains a file matching
/// the cue. Returns the cohort's real file paths (the structural template the
/// agent should mirror for a NEW feature). Generic — learns the repo's own
/// convention; no hardcoded layout. `index` = web-root-stripped paths in their
/// ORIGINAL case (case matters for the interface `I`-prefix detection); grouping
/// keys are lowercased internally and the cue match is case-insensitive.
/// Pick the scaffold entity from the story by grounding it in the index:
/// the longest story n-gram (3→1 words) whose compact form names an
/// existing path segment wins — a new API domain almost always mirrors an
/// existing page/dir name (markerinspection page → MarkerInspection API),
/// so "marker inspection" beats the bare concept "inspection".
fn derive_scaffold_entity(story: &str, index: &[String]) -> Option<String> {
    let words: Vec<String> = story
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3)
        .map(str::to_string)
        .collect();
    let seg_set: std::collections::HashSet<String> = index
        .iter()
        .flat_map(|p| p.split('/'))
        .map(|s| s.split('.').next().unwrap_or(s).to_lowercase())
        .collect();
    // Candidates from BOTH passes compete; the most SPECIFIC (longest)
    // grounded segment wins. Returning early on an exact unigram match
    // ("inspection") hid the more specific feature dir (markerinspection).
    let mut best: Option<(usize, String)> = None; // (segment len, pascal)
    let mut offer = |len: usize, pascal: String| {
        if best.as_ref().is_none_or(|(l, _)| len > *l) {
            best = Some((len, pascal));
        }
    };
    let cap = |s: &str| -> String {
        let mut s = s.to_string();
        if let Some(f) = s.get_mut(0..1) {
            f.make_ascii_uppercase();
        }
        s
    };
    // Pass 1: exact story n-gram = path segment.
    for n in (1..=3usize).rev() {
        if words.len() < n {
            continue;
        }
        for win in words.windows(n) {
            let compact: String = win.concat();
            if compact.len() >= 6 && seg_set.contains(&compact) {
                offer(compact.len(), win.iter().map(|w| cap(w)).collect());
            }
        }
    }
    // Pass 2: the story often names the DOMAIN indirectly ("the Inspection
    // module in the dashboard") while the codebase dir is more specific
    // (markerinspection). Feature DIRECTORIES (≥2 files under them) whose
    // name CONTAINS a story word count too — but only when the matched
    // word covers at least half the segment, so "report" cannot claim
    // reportingofquantities.
    let mut dir_counts: std::collections::HashMap<&str, usize> = Default::default();
    for p in index {
        if let Some((dirs, _file)) = p.rsplit_once('/') {
            for seg in dirs.split('/') {
                *dir_counts.entry(seg).or_default() += 1;
            }
        }
    }
    for (seg, count) in dir_counts {
        if count < 2 || seg.len() < 8 {
            continue;
        }
        let seg_l = seg.to_lowercase();
        for w in &words {
            if w.len() < 6
                || seg_l == *w
                || !seg_l.contains(w.as_str())
                || w.len() * 2 < seg_l.len()
            {
                continue;
            }
            let idx = seg_l.find(w.as_str()).unwrap_or(0);
            let pascal = format!(
                "{}{}{}",
                cap(&seg_l[..idx]),
                cap(w),
                cap(&seg_l[idx + w.len()..])
            );
            offer(seg.len(), pascal);
        }
    }
    best.map(|(_, p)| p)
}

/// Parameterize a scaffold template with the story's entity: rewrite each
/// cohort path's domain directory (the camelCase feature dir) and basename
/// entity stem so the agent gets the CONCRETE files to create, not just an
/// example family to reverse-engineer. PR1890 showed agents (and the
/// dossier recall metric) need the target paths spelled out.
fn propose_scaffold_paths(cohort: &[String], entity_pascal: &str) -> Vec<String> {
    if entity_pascal.is_empty() {
        return Vec::new();
    }
    let mut camel = entity_pascal.to_string();
    if let Some(c) = camel.get_mut(0..1) {
        c.make_ascii_lowercase();
    }
    const ROLE_DIRS: [&str; 8] = [
        "app_code",
        "api-v2",
        "controllers",
        "services",
        "datatransferobjects",
        "queryparams",
        "interfaces",
        "site",
    ];
    const MARKERS: [&str; 5] = ["Controller", "Service", "Query", "-In", "-Out"];
    let mut out = Vec::new();
    for f in cohort {
        let mut segs: Vec<String> = f.split('/').map(str::to_string).collect();
        let Some(base) = segs.pop() else { continue };
        // Replace the feature-domain directory segment.
        for s in segs.iter_mut() {
            if !ROLE_DIRS.contains(&s.to_lowercase().as_str())
                && s.chars().next().is_some_and(|c| c.is_lowercase())
            {
                *s = camel.clone();
            }
        }
        // Rewrite the basename's entity stem, keeping the role suffix.
        let (stem, ext) = base.split_once('.').unwrap_or((base.as_str(), "vb"));
        let mut new_base = None;
        for m in MARKERS {
            if let Some(idx) = stem.find(m) {
                let prefix = &stem[..idx];
                let iface = prefix.len() >= 2
                    && prefix.starts_with('I')
                    && prefix.chars().nth(1).is_some_and(|c| c.is_uppercase());
                let plural = m == "Controller" && prefix.ends_with('s');
                new_base = Some(format!(
                    "{}{}{}{}.{}",
                    if iface { "I" } else { "" },
                    entity_pascal,
                    if plural { "s" } else { "" },
                    &stem[idx..],
                    ext
                ));
                break;
            }
        }
        let Some(nb) = new_base else { continue };
        segs.push(nb);
        out.push(segs.join("/"));
    }
    out.sort();
    out.dedup();
    out
}

/// Pick the template family for the scaffold section. `ranked` is the
/// story's own candidate set (canon lowercase paths): the family MOST
/// CONNECTED to this story wins — both more relevant as a template and
/// fully deterministic. The old (dirs, file-count) ordering left ties to
/// HashMap iteration order, so the SAME dossier flipped template families
/// across runs (live PR1890: RoQ vs User family, recall tri-stating
/// 54/62/69 on the flip).
fn find_analog_cohort(
    index: &[String],
    cue: &str,
    ranked: &HashSet<String>,
) -> Option<Vec<String>> {
    let cue = cue.to_lowercase();
    let mut groups: HashMap<(String, String), (HashSet<String>, Vec<String>)> = HashMap::new();
    for f in index {
        let segs: Vec<&str> = f.split('/').filter(|s| !s.is_empty()).collect();
        if segs.len() < 3 {
            continue;
        }
        let area2 = format!("{}/{}", segs[0].to_lowercase(), segs[1].to_lowercase());
        let fname = segs[segs.len() - 1];
        let base = fname.rsplit_once('.').map(|(b, _)| b).unwrap_or(fname);
        let stem = scaffold_stem(base); // uses original case for the I-prefix rule
        if stem.len() < 4 {
            continue;
        }
        let parent = segs[..segs.len() - 1].join("/").to_lowercase();
        let e = groups.entry((area2, stem)).or_default();
        e.0.insert(parent);
        e.1.push(f.clone());
    }
    let mut cands: Vec<(usize, usize, Vec<String>)> = groups
        .into_values()
        .filter(|(dirs, files)| {
            dirs.len() >= 3
                && (cue.is_empty() || files.iter().any(|f| f.to_lowercase().contains(&cue)))
        })
        .map(|(dirs, mut files)| {
            files.sort();
            let overlap = files
                .iter()
                .filter(|f| ranked.contains(&f.to_lowercase()))
                .count();
            (overlap, dirs.len(), files)
        })
        .collect();
    cands.sort_by(|a, b| {
        b.0.cmp(&a.0) // candidate-set overlap: the story's OWN family first
            .then(b.1.cmp(&a.1)) // then structural completeness (role dirs)
            .then(b.2.len().cmp(&a.2.len()))
            .then_with(|| a.2.cmp(&b.2)) // lexicographic: NEVER map order
    });
    cands.into_iter().next().map(|(_, _, files)| files)
}

/// Co-change-first tier for ranking: history/co-change (the most predictive
/// signal) ranks above multi-arm, above concept-only, above graph-only.
fn change_set_tier(sigs: &BTreeSet<&'static str>) -> u8 {
    // Evidence DIRECTNESS, not signal count (row-1 audit A2): golden
    // (co-change/history) first; then an entity match corroborated by an
    // associative signal; then the entity match alone; two associative
    // signals (vector/graph) never outrank a precise concept hit.
    // External audit 2026-08-29 P0-3: a file matching the story's explicit
    // gloss is the entity the author named — as direct as history.
    let golden = sigs.contains("cochange")
        || sigs.contains("history")
        || sigs.contains("gloss")
        || sigs.contains("lexicon");
    let concept = sigs.contains("concept");
    if golden && sigs.len() >= 2 {
        0
    } else if golden {
        1
    } else if concept && sigs.len() >= 2 {
        2
    } else if concept {
        3
    } else {
        4
    }
}

/// Split an identifier into lowercase tokens on camelCase, snake_case and
/// kebab-case boundaries (plus letter/digit transitions), handling acronym
/// runs: `ddlBillingStatusMainContractor` -> [ddl, billing, status, main,
/// contractor]; `btn_from_date` -> [btn, from, date].
pub(crate) fn split_symmetric_name_tokens(name: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for part in name.split(|c: char| !c.is_alphanumeric()) {
        if part.is_empty() {
            continue;
        }
        let chars: Vec<char> = part.chars().collect();
        let mut cur = String::new();
        for (i, &c) in chars.iter().enumerate() {
            let boundary = i > 0
                && ((c.is_uppercase() && chars[i - 1].is_lowercase())
                    // Acronym-run end: "HTMLParser" -> HTML | Parser.
                    || (c.is_uppercase()
                        && chars[i - 1].is_uppercase()
                        && chars.get(i + 1).is_some_and(|n| n.is_lowercase()))
                    || (c.is_ascii_digit() != chars[i - 1].is_ascii_digit()));
            if boundary && !cur.is_empty() {
                tokens.push(cur.to_lowercase());
                cur.clear();
            }
            cur.push(c);
        }
        if !cur.is_empty() {
            tokens.push(cur.to_lowercase());
        }
    }
    tokens
}

/// Symmetric sibling pairs: two identifiers pair when their token sequences
/// (camelCase/snake_case split) have EQUAL length and differ in EXACTLY ONE
/// position, sharing at least 2 tokens. This is the name-shape of PAIRED
/// WebForms/WinForms controls and handlers (Main/Sub, Left/Right, From/To,
/// Start/End, Sender/Receiver) — a change to one side usually requires
/// deciding the twin's behavior. Pairs are DISJOINT (greedy: each name joins
/// at most ONE pair) — a single symmetric FAMILY like Show/Hide × Main/Sub
/// otherwise fans out into C(n,2) redundant cross-pairs that crowd every
/// other family past the cap (the a live pilot PR miss: Hide/Show chains starved
/// the ddl…Main/Sub handler pair the story was actually about). Dedupes
/// (a,b)/(b,a); capped at 6 pairs.
/// Relevance tier of a symmetric pair from its ONE differing token pair.
/// 2 = a real MUTUAL-EXCLUSION / directional antonym (Main/Sub, Show/Hide,
/// Enable/Disable, Left/Right, …) — the toggle a story like "show X when
/// Y is selected" actually needs; 0 = a CRUD/LIFECYCLE antonym (Add/Clear,
/// Commit/Dispose, Load/Save) that name-symmetry catches but that is rarely
/// the story's twin-decision; 1 = everything else (Company/Customer,
/// Period/Project). Keeps the high-signal toggle pairs above the lifecycle
/// noise the flat first-N order used to mix in (live: PR1933 dossier).
fn sibling_pair_tier(a: &str, b: &str) -> u8 {
    const TOGGLE: &[&str] = &[
        "main", "sub", "show", "hide", "enable", "disable", "left", "right", "from", "to", "start",
        "end", "open", "close", "prev", "previous", "next", "expand", "collapse", "on", "off",
        "min", "max", "up", "down", "first", "last", "in", "out", "single", "multi", "line",
        "polygon", "point", "checkin", "checkout", "before", "after", "include", "exclude", "asc",
        "desc",
    ];
    const LIFECYCLE: &[&str] = &[
        "add",
        "remove",
        "clear",
        "delete",
        "insert",
        "create",
        "commit",
        "dispose",
        "load",
        "save",
        "attach",
        "detach",
        "init",
        "destroy",
        "connect",
        "disconnect",
        "copy",
        "move",
        "import",
        "export",
    ];
    let (a, b) = (a.to_lowercase(), b.to_lowercase());
    if TOGGLE.contains(&a.as_str()) && TOGGLE.contains(&b.as_str()) {
        2
    } else if LIFECYCLE.contains(&a.as_str()) || LIFECYCLE.contains(&b.as_str()) {
        0
    } else {
        1
    }
}

/// Relevance tier of an already-formed symmetric pair (two full names),
/// for cross-file ranking in the sibling section. Splits both, finds the
/// single differing token, and tiers it (see sibling_pair_tier). Falls
/// back to tier 1 when the names aren't cleanly one token apart.
pub(crate) fn symmetric_pair_tier(a: &str, b: &str) -> u8 {
    let ta = split_symmetric_name_tokens(a);
    let tb = split_symmetric_name_tokens(b);
    if ta.len() != tb.len() {
        return 1;
    }
    let diffs: Vec<(&String, &String)> = ta.iter().zip(tb.iter()).filter(|(x, y)| x != y).collect();
    if diffs.len() == 1 {
        sibling_pair_tier(diffs[0].0, diffs[0].1)
    } else {
        1
    }
}

pub(crate) fn symmetric_sibling_pairs(names: &[String]) -> Vec<(String, String)> {
    const MAX_PAIRS: usize = 6;
    let mut seen: HashSet<&str> = HashSet::new();
    let uniq: Vec<&String> = names.iter().filter(|n| seen.insert(n.as_str())).collect();
    let toks: Vec<Vec<String>> = uniq
        .iter()
        .map(|n| split_symmetric_name_tokens(n))
        .collect();
    let mut used = vec![false; uniq.len()];
    // Collect ALL pairs with their relevance tier, then sort so the
    // toggle/directional pairs win the MAX_PAIRS budget over lifecycle
    // noise (was: first-N in name order, which mixed them).
    let mut scored: Vec<(u8, usize, String, String)> = Vec::new();
    for i in 0..uniq.len() {
        if used[i] || toks[i].len() < 3 {
            continue;
        }
        for j in (i + 1)..uniq.len() {
            if used[j] || toks[i].len() != toks[j].len() {
                continue;
            }
            let diffs: Vec<(&String, &String)> = toks[i]
                .iter()
                .zip(toks[j].iter())
                .filter(|(a, b)| a != b)
                .collect();
            if diffs.len() == 1 {
                let (da, db) = diffs[0];
                let tier = sibling_pair_tier(da, db);
                scored.push((tier, i, uniq[i].clone(), uniq[j].clone()));
                used[i] = true;
                used[j] = true;
                break;
            }
        }
    }
    // Tier desc; within a tier preserve discovery order (stable by index).
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    scored
        .into_iter()
        .take(MAX_PAIRS)
        .map(|(_, _, a, b)| (a, b))
        .collect()
}

/// Top provisional files by corroboration tier that can actually CONTAIN
/// graph nodes (controls/functions/classes) — the shared "top-N files worth
/// a node scan" pick for every dossier section that queries per-file nodes
/// (symmetric siblings, permission gates, …). Encodes two live traps (the
/// PR1933 regression where the sibling section silently vanished):
///  • extension filter — resource/data files carry golden tags too (a
///    localized .resx family inherits its seed's co-change signal, making
///    every language sibling tier 0) and `App_GlobalResources/…` sorts
///    alphabetically FIRST, so without the filter the entire scan budget went
///    to .resx files that have no control/function nodes → zero pairs;
///  • corroboration tiebreak — within a tier, MORE signals first, so the
///    story's strongest file (e.g. cochange|history|vector) is scanned before
///    single-corroboration peers instead of being buried by BTreeMap
///    alphabetical order ("site/…" sorts last).
fn top_node_bearing_files(
    prov: &BTreeMap<String, BTreeSet<&'static str>>,
    take: usize,
) -> Vec<String> {
    const CODE_NODE_EXTS: &[&str] = &[
        ".vb", ".cs", ".aspx", ".ascx", ".master", ".asmx", ".ashx", ".svc", ".asax", ".ts",
        ".tsx", ".js", ".jsx", ".vbhtml", ".cshtml",
    ];
    let mut ranked: Vec<(&String, &BTreeSet<&'static str>)> = prov
        .iter()
        .filter(|(p, _)| {
            let pl = p.to_lowercase();
            CODE_NODE_EXTS.iter().any(|e| pl.ends_with(e))
        })
        .collect();
    ranked.sort_by(|a, b| {
        change_set_tier(a.1)
            .cmp(&change_set_tier(b.1))
            .then_with(|| b.1.len().cmp(&a.1.len()))
            .then_with(|| a.0.cmp(b.0))
    });
    ranked
        .into_iter()
        .take(take)
        .map(|(p, _)| p.clone())
        .collect()
}

#[cfg(test)]
mod symmetric_sibling_tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn tokenizer_splits_camel_snake_and_acronyms() {
        assert_eq!(
            split_symmetric_name_tokens("ddlBillingStatusMainContractor"),
            vec!["ddl", "billing", "status", "main", "contractor"]
        );
        assert_eq!(
            split_symmetric_name_tokens("btn_from_date"),
            vec!["btn", "from", "date"]
        );
        assert_eq!(
            split_symmetric_name_tokens("HTMLParser"),
            vec!["html", "parser"]
        );
    }

    #[test]
    fn pairs_camel_case_one_token_apart() {
        let pairs = symmetric_sibling_pairs(&names(&[
            "ddlBillingStatusMainContractor",
            "ddlBillingStatusSubContractor",
        ]));
        assert_eq!(
            pairs,
            vec![(
                "ddlBillingStatusMainContractor".to_string(),
                "ddlBillingStatusSubContractor".to_string()
            )]
        );
    }

    #[test]
    fn pairs_snake_case_one_token_apart() {
        let pairs = symmetric_sibling_pairs(&names(&["btn_from_date", "btn_to_date"]));
        assert_eq!(
            pairs,
            vec![("btn_from_date".to_string(), "btn_to_date".to_string())]
        );
    }

    #[test]
    fn symmetric_pair_tier_classifies_full_names() {
        use super::symmetric_pair_tier;
        // Toggle/directional antonyms → tier 2.
        assert_eq!(
            symmetric_pair_tier("ToggleFilterMainContractor", "ToggleFilterSubContractor"),
            2
        );
        assert_eq!(
            symmetric_pair_tier("ShowBillingPanel", "HideBillingPanel"),
            2
        );
        // CRUD/lifecycle → tier 0.
        assert_eq!(symmetric_pair_tier("svcAddRow", "svcClearRow"), 0);
        assert_eq!(symmetric_pair_tier("Commit", "Dispose"), 0);
        // Neutral → tier 1.
        assert_eq!(symmetric_pair_tier("CompanyName", "CustomerName"), 1);
    }

    #[test]
    fn toggle_pairs_outrank_lifecycle_under_the_cap() {
        // 7 pairs, cap is 6: the lifecycle Add/Clear pair must be the one
        // dropped, and Main/Sub + Show/Hide must lead (PR1933 relevance).
        let mut input: Vec<&str> = Vec::new();
        // 6 lifecycle pairs (tier 0) declared FIRST so name-order would
        // have kept them under the old first-N logic.
        for (a, b) in [
            ("svcAddRow", "svcClearRow"),
            ("svcCommitTx", "svcDisposeTx"),
            ("svcLoadState", "svcSaveState"),
            ("svcAttachItem", "svcDetachItem"),
            ("svcImportData", "svcExportData"),
            ("svcConnectDb", "svcDisconnectDb"),
        ] {
            input.push(a);
            input.push(b);
        }
        // 2 toggle pairs (tier 2) declared LAST.
        input.extend(["ToggleFilterMainContractor", "ToggleFilterSubContractor"]);
        input.extend(["ShowBillingPanel", "HideBillingPanel"]);
        let pairs = symmetric_sibling_pairs(&names(&input));
        assert_eq!(pairs.len(), 6);
        let flat: Vec<String> = pairs
            .iter()
            .flat_map(|(a, b)| [a.clone(), b.clone()])
            .collect();
        // Both toggle pairs survive the cap...
        assert!(flat.iter().any(|n| n.contains("MainContractor")));
        assert!(flat.iter().any(|n| n.contains("ShowBillingPanel")));
        // ...and they lead (first two emitted pairs are the tier-2 toggles).
        assert!(pairs[0].0.contains("Main") || pairs[0].0.contains("Show"));
        assert!(pairs[1].0.contains("Main") || pairs[1].0.contains("Show"));
    }

    #[test]
    fn rejects_unrelated_and_multi_token_differences() {
        // Unrelated names: no pair.
        assert!(symmetric_sibling_pairs(&names(&["btnSave", "gvOrders", "lblTitle"])).is_empty());
        // Two differing token positions: no pair.
        assert!(
            symmetric_sibling_pairs(&names(&[
                "ddlBillingStatusMainContractor",
                "ddlPaymentStatusSubContractor"
            ]))
            .is_empty()
        );
        // Different token counts: no pair.
        assert!(
            symmetric_sibling_pairs(&names(&["btn_from_date", "btn_to_date_extra"])).is_empty()
        );
        // Only 2 tokens => fewer than 2 shared tokens: no pair.
        assert!(symmetric_sibling_pairs(&names(&["btnFrom", "btnTo"])).is_empty());
    }

    #[test]
    fn dedupes_names_and_caps_at_six_pairs() {
        // Duplicate name must not pair with itself.
        assert!(symmetric_sibling_pairs(&names(&["btn_from_date", "btn_from_date"])).is_empty());
        // Pairs are DISJOINT: 8 mutually-pairing names (differ only in the
        // middle token) yield 4 greedy pairs, not C(8,2) cross-products.
        let many: Vec<String> = (0..8).map(|i| format!("pnlFilterVariant{i}Side")).collect();
        let pairs = symmetric_sibling_pairs(&many);
        assert_eq!(pairs.len(), 4);
        let mut used: HashSet<&String> = HashSet::new();
        for (a, b) in &pairs {
            assert!(
                used.insert(a) && used.insert(b),
                "name appears twice: {pairs:?}"
            );
            assert!(many.iter().position(|n| n == a) < many.iter().position(|n| n == b));
        }
        // Cap holds at 6: 14 mutually-pairing names = 7 disjoint pairs -> 6.
        let many: Vec<String> = (0..14)
            .map(|i| format!("pnlFilterVariant{i}Side"))
            .collect();
        assert_eq!(symmetric_sibling_pairs(&many).len(), 6);
    }

    #[test]
    fn live_producedq_ddl_handler_pair_survives_hide_show_family() {
        // The exact a live pilot PR shape: producedq.aspx.vb's Show/Hide × Main/Sub
        // helper family plus the ddl…Main/Sub SelectedIndexChanged handlers
        // (graph function names are class-qualified). Under all-pairs
        // enumeration the Hide/Show cross-pairs starved the ddl pair past the
        // cap; disjoint pairing must keep ONE pair per family and include the
        // handler pair the story is actually about.
        let ns = names(&[
            "producedq_producedq.DisableFilterDateOnOptions",
            "producedq_producedq.EnableFilterDateOnOptions",
            "producedq_producedq.HideMainContractorBillingFilter",
            "producedq_producedq.HideSubContractorBillingFilter",
            "producedq_producedq.ShowMainContractorBillingFilter",
            "producedq_producedq.ShowSubContractorBillingFilter",
            "producedq_producedq.ddlBillingStatusMainContractor_SelectedIndexChanged",
            "producedq_producedq.ddlBillingStatusSubContractor_SelectedIndexChanged",
        ]);
        let pairs = symmetric_sibling_pairs(&ns);
        assert_eq!(pairs.len(), 4, "{pairs:?}");
        assert!(
            pairs.contains(&(
                "producedq_producedq.ddlBillingStatusMainContractor_SelectedIndexChanged"
                    .to_string(),
                "producedq_producedq.ddlBillingStatusSubContractor_SelectedIndexChanged"
                    .to_string()
            )),
            "{pairs:?}"
        );
        // Unqualified control-node names pair the same way.
        let pairs = symmetric_sibling_pairs(&names(&[
            "ddlBillingStatusMainContractor",
            "ddlBillingStatusSubContractor",
        ]));
        assert_eq!(pairs.len(), 1, "{pairs:?}");
    }

    #[test]
    fn scan_files_skip_noncode_and_rank_by_corroboration() {
        // The a live pilot PR regression: the resx family expansion makes every
        // language sibling {cochange, family} = tier 0, and
        // app_globalresources/… sorts alphabetically before every code file,
        // so an unfiltered top-10 was ALL resource files (no control/function
        // nodes) and the section silently vanished. The scan set must skip
        // node-less extensions and rank the strongest code file first.
        let mut prov: BTreeMap<String, BTreeSet<&'static str>> = BTreeMap::new();
        for lang in [
            "de", "en", "es", "no", "pt", "sl", "sv", "da", "fi", "fr", "it", "nl", "pl",
        ] {
            prov.insert(
                format!("app_globalresources/systemsettings.{lang}.resx"),
                BTreeSet::from(["cochange", "family"]),
            );
        }
        prov.insert(
            "db-app.sql/scripts/post/ss_systemsettings.sql".into(),
            BTreeSet::from(["cochange", "concept"]),
        );
        prov.insert(
            "site/modules/dashboard/pages/public/producedq/producedq.aspx.vb".into(),
            BTreeSet::from(["cochange", "history", "vtop"]),
        );
        prov.insert(
            "site/app_code/ifalt.designer.vb".into(),
            BTreeSet::from(["cochange", "concept"]),
        );
        prov.insert(
            "app_code/shared-code/configsettings.vb".into(),
            BTreeSet::from(["cochange"]),
        );
        let files = top_node_bearing_files(&prov, 10);
        assert!(
            files
                .iter()
                .all(|f| !f.ends_with(".resx") && !f.ends_with(".sql")),
            "{files:?}"
        );
        // Tier 0 with THREE signals outranks tier-0 two-signal peers despite
        // sorting alphabetically last.
        assert_eq!(
            files[0],
            "site/modules/dashboard/pages/public/producedq/producedq.aspx.vb"
        );
        assert!(files.contains(&"app_code/shared-code/configsettings.vb".to_string()));
    }

    #[test]
    fn permission_scan_top_files_survive_resx_flood() {
        // The permission-gates section shape: it used to take the first 15
        // prov keys ALPHABETICALLY, so a localized resx family (16 languages,
        // App_GlobalResources/… sorts before every code path) consumed the
        // entire node-scan budget — resx files carry no permission_checks/
        // guard_roles metadata, and the section silently vanished. The shared
        // pick must skip the flood and keep the node-bearing code files.
        let mut prov: BTreeMap<String, BTreeSet<&'static str>> = BTreeMap::new();
        for lang in [
            "cs", "da", "de", "en", "es", "et", "fi", "fr", "it", "lt", "lv", "nl", "no", "pl",
            "pt", "sv",
        ] {
            prov.insert(
                format!("App_GlobalResources/producedq.aspx.{lang}.resx"),
                BTreeSet::from(["cochange", "family"]),
            );
        }
        prov.insert(
            "site/app_code/security/role.vb".into(),
            BTreeSet::from(["cochange", "concept"]),
        );
        prov.insert(
            "site/modules/dashboard/pages/public/producedq/producedq.aspx".into(),
            BTreeSet::from(["cochange", "history"]),
        );
        prov.insert(
            "scripts/map/mapfilters.ts".into(),
            BTreeSet::from(["concept"]),
        );
        let files = top_node_bearing_files(&prov, 15);
        assert!(files.iter().all(|f| !f.ends_with(".resx")), "{files:?}");
        assert!(files.contains(&"site/app_code/security/role.vb".to_string()));
        assert!(
            files.contains(
                &"site/modules/dashboard/pages/public/producedq/producedq.aspx".to_string()
            )
        );
        assert!(files.contains(&"scripts/map/mapfilters.ts".to_string()));
    }
}

/// Render the change set: layer-grouped, co-change-first, with a completeness
/// checklist. Golden (co-change/history) files are never capped; the concept/
/// graph tail is capped per layer so flood can't crowd out real companions.
/// Layers of the change set (extension-based). ONE definition shared by the
/// markdown renderer and the JSON payload so they cannot drift.
pub(crate) const CHANGE_SET_LAYERS: &[(&str, &[&str])] = &[
    (
        "Server (VB / code-behind / markup)",
        &[
            ".vb", ".cs", ".aspx", ".ascx", ".master", ".asmx", ".ashx", ".svc", ".asax",
        ],
    ),
    (
        "Client (TypeScript / JavaScript)",
        &[".ts", ".tsx", ".js", ".jsx"],
    ),
    ("Resources (.resx — translate EVERY language)", &[".resx"]),
    ("Data (SQL)", &[".sql"]),
    (
        "Markup / styles / config",
        &[".html", ".css", ".config", ".vbhtml", ".cshtml"],
    ),
];

pub(crate) fn change_set_layer_index(p: &str) -> usize {
    let pl = p.to_lowercase();
    for (i, (_, exts)) in CHANGE_SET_LAYERS.iter().enumerate() {
        if exts.iter().any(|e| pl.ends_with(*e)) {
            return i;
        }
    }
    CHANGE_SET_LAYERS.len()
}

pub(crate) fn change_set_layer_name(i: usize) -> &'static str {
    CHANGE_SET_LAYERS.get(i).map(|(n, _)| *n).unwrap_or("Other")
}

/// Per-layer cap on WEAK-signal candidates (tier ≥ 2, not `vtop`/`family`).
/// The eval sweet spot (45cf172); what it cuts is now REPORTED as omissions.
pub(crate) const CHANGE_SET_TAIL_CAP: usize = 18;

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ChangeSetOmission {
    pub path: String,
    pub layer: &'static str,
    pub reason: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ChangeSetRow {
    pub path: String,
    pub layer: &'static str,
    pub layer_index: usize,
    pub tier: u8,
    pub signals: Vec<&'static str>,
    pub omitted: bool,
}

/// Ranked rows in render order (layer, tier, depth, path) with the tail-cap
/// decision applied — the single place both renderers read from.
pub(crate) fn change_set_rows(
    prov: &BTreeMap<String, BTreeSet<&'static str>>,
) -> (Vec<ChangeSetRow>, Vec<ChangeSetOmission>) {
    let mut rows = Vec::new();
    let mut omissions = Vec::new();
    for li in 0..=CHANGE_SET_LAYERS.len() {
        let mut items: Vec<(&String, &BTreeSet<&'static str>)> = prov
            .iter()
            .filter(|(p, _)| change_set_layer_index(p) == li)
            .collect();
        if items.is_empty() {
            continue;
        }
        items.sort_by(|a, b| {
            change_set_tier(a.1)
                .cmp(&change_set_tier(b.1))
                .then(a.0.matches('/').count().cmp(&b.0.matches('/').count()))
                .then(a.0.cmp(b.0))
        });
        let lname = change_set_layer_name(li);
        let mut tail = 0usize;
        for (p, sigs) in items {
            let tier = change_set_tier(sigs);
            let exempt = sigs.contains("vtop")
                || sigs.contains("family")
                || sigs.contains("gloss")
                || sigs.contains("lexicon");
            let mut omitted = false;
            if tier >= 2 && !exempt {
                tail += 1;
                if tail > CHANGE_SET_TAIL_CAP {
                    omitted = true;
                    omissions.push(ChangeSetOmission {
                        path: p.clone(),
                        layer: lname,
                        reason: format!(
                            "weak-signal tail cap ({CHANGE_SET_TAIL_CAP} per layer) in '{lname}'"
                        ),
                    });
                }
            }
            let signals: Vec<&'static str> = sigs
                .iter()
                .filter(|s| **s != "family")
                .map(|s| if *s == "vtop" { "vector" } else { *s })
                .collect();
            rows.push(ChangeSetRow {
                path: p.clone(),
                layer: lname,
                layer_index: li,
                tier,
                signals,
                omitted,
            });
        }
    }
    (rows, omissions)
}

/// What one retrieval arm of get_change_set delivered.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub(crate) struct ArmCoverage {
    /// `complete` | `truncated` | `failed` | `not_run`
    pub status: String,
    pub hits: usize,
    pub ms: u128,
    pub note: String,
}

impl ArmCoverage {
    fn complete(hits: usize, ms: u128) -> Self {
        Self {
            status: "complete".into(),
            hits,
            ms,
            note: String::new(),
        }
    }
    fn truncated(hits: usize, ms: u128, note: String) -> Self {
        Self {
            status: "truncated".into(),
            hits,
            ms,
            note,
        }
    }
    fn failed(note: String, ms: u128) -> Self {
        Self {
            status: "failed".into(),
            hits: 0,
            ms,
            note,
        }
    }
    fn not_run(note: &str) -> Self {
        Self {
            status: "not_run".into(),
            hits: 0,
            ms: 0,
            note: note.into(),
        }
    }
    fn line(&self) -> String {
        let mut l = format!("{} ({} hits, {} ms)", self.status, self.hits, self.ms);
        if !self.note.is_empty() {
            l.push_str(" — ");
            l.push_str(&self.note);
        }
        l
    }
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub(crate) struct ChangeSetCoverage {
    pub concept: ArmCoverage,
    pub history: ArmCoverage,
    pub cochange: ArmCoverage,
    pub vector: ArmCoverage,
    pub kb_bridge: ArmCoverage,
    pub family: ArmCoverage,
    /// Full project node scans performed by this call (audit D7: the
    /// repeated detect_incomplete_changes passes used to re-scan 200k nodes
    /// each; they now share one snapshot).
    pub node_scans: usize,
    /// Index-corroborated entity names found in the story (advisory unless
    /// `expand_concepts` is set): the three recipe concepts first, then the
    /// resolved extras.
    pub concept_candidates: Vec<String>,
    /// External audit 2026-08-29 P0-3: the gloss-derived concepts that
    /// retrieved by default (a subset of `concept_candidates`).
    #[serde(default)]
    pub gloss_concepts: Vec<String>,
    /// External audit 2026-08-29 P0-3 (≤ 5 s gate): wall-clock of the call and
    /// cumulative checkpoints (ms since start) — `node_scan`, `arms_done`,
    /// `before_render`, `render` — so the time outside the timed arms is
    /// visible instead of inferred.
    #[serde(default)]
    pub wall_ms: u128,
    #[serde(default)]
    pub stages: std::collections::BTreeMap<String, u128>,
    /// External audit 2026-08-29 row 1: `english phrase → swedish term` pairs the
    /// project's .resx lexicon contributed as default concepts.
    #[serde(default)]
    pub lexicon_concepts: Vec<String>,
}

/// A full project node scan shared across the sub-calls of one request.
pub(crate) type NodeSnapshot = std::sync::Arc<Vec<engram_graph::Node>>;

fn render_change_set_coverage(cov: &ChangeSetCoverage, omitted: usize) -> String {
    let mut s = String::from("\n## Coverage\n");
    s.push_str(&format!("- concept: {}\n", cov.concept.line()));
    s.push_str(&format!("- history: {}\n", cov.history.line()));
    s.push_str(&format!("- co-change: {}\n", cov.cochange.line()));
    s.push_str(&format!("- vector: {}\n", cov.vector.line()));
    s.push_str(&format!("- kb bridge: {}\n", cov.kb_bridge.line()));
    if cov.wall_ms > 0 {
        let cps: Vec<String> = cov.stages.iter().map(|(k, v)| format!("{k} {v}")).collect();
        s.push_str(&format!(
            "- wall: {} ms (checkpoints: {})\n",
            cov.wall_ms,
            cps.join(", ")
        ));
    }
    s.push_str(&format!("- family: {}\n", cov.family.line()));
    s.push_str(&format!("- node scans: {}\n", cov.node_scans));
    if !cov.lexicon_concepts.is_empty() {
        s.push_str(&format!(
            "- lexicon (the project's .resx EN→SV pairs) translated the story: {}\n",
            cov.lexicon_concepts.join(", ")
        ));
    }
    if !cov.gloss_concepts.is_empty() {
        s.push_str(&format!(
            "- explicit story gloss(es) retrieved by default: {}\n",
            cov.gloss_concepts.join(", ")
        ));
    }
    if cov.concept_candidates.len() > 3 {
        s.push_str(&format!(
            "- entity candidates (index-corroborated, advisory — expand_concepts=true to retrieve on them): {}\n",
            cov.concept_candidates[3..].join(", ")
        ));
    }
    if omitted > 0 {
        s.push_str(&format!(
            "- omitted by the per-layer tail cap: {omitted} (paths in output_json.omissions)\n"
        ));
    }
    s
}

fn render_change_set(
    story: &str,
    concepts: &[String],
    prov: &BTreeMap<String, BTreeSet<&'static str>>,
    temporal_section: Option<&str>,
    sibling_section: Option<&str>,
    setting_prior: Option<(usize, usize)>,
    historical: &BTreeSet<String>,
) -> (String, Vec<ChangeSetOmission>) {
    const LAYERS: &[(&str, &[&str])] = &[
        (
            "Server (VB / code-behind / markup)",
            &[
                ".vb", ".cs", ".aspx", ".ascx", ".master", ".asmx", ".ashx", ".svc", ".asax",
            ],
        ),
        (
            "Client (TypeScript / JavaScript)",
            &[".ts", ".tsx", ".js", ".jsx"],
        ),
        ("Resources (.resx — translate EVERY language)", &[".resx"]),
        ("Data (SQL)", &[".sql"]),
        (
            "Markup / styles / config",
            &[".html", ".css", ".config", ".vbhtml", ".cshtml"],
        ),
    ];
    let mut s = String::new();
    s.push_str("# Change set — candidate files for this story\n\n");
    s.push_str(&format!(
        "story: {story}\nconcepts: {}\n\n",
        concepts.join(", ")
    ));
    s.push_str(
        "Ranked by corroboration — git CO-CHANGE / history first (files that \
         historically shipped together with this kind of work), then concept/graph. \
         A starting map, not exhaustive: verify each against the code and add the \
         files it misses.\n\n",
    );
    // A live A/B showed strong planners SKIP ranked candidates whose signal
    // they don't understand (the source page edit was ranked and still
    // dropped). Spell out what each tag proves and raise the bar for
    // skipping the evidence-backed ones.
    s.push_str(
        "Signal legend — [cochange]: this file SHIPPED TOGETHER with other \
         candidates in past MERGED changes of this kind (strongest evidence; \
         skipping one requires POSITIVE evidence of irrelevance grounded in the code - and a .ts with its committed .js bundle listed together is part of the change until PROVEN otherwise (a live A/B dismissed exactly that pair as bleed-through; it was real)). [history]: past \
         commits in this story's domain touched it. [concept]: name/content \
         matches the story's concepts. [semantic]/[graph]: embedding or \
         dependency-graph association (weakest — verify before trusting).\n\n",
    );
    // Temporal-analytics section renders BEFORE the checklist so the
    // checklist's "log/history tables above" pointer is literally true.
    if let Some(sec) = temporal_section {
        s.push_str(sec);
    }
    // Symmetric-sibling section likewise renders BEFORE the checklist so the
    // checklist's "pair(s) above" pointer is literally true.
    if let Some(sec) = sibling_section {
        s.push_str(sec);
    }
    s.push_str(
        "## Completeness checklist - REQUIRED decision points\n\
         Treat EACH item below as a decision you must make and state \
         explicitly (the highest-scoring plans in live A/Bs stated every \
         decision; silent omissions were the top failure mode):\n",
    );
    s.push_str(
        "- Every page touched: edit BOTH the .aspx/.ascx markup AND its \
         .aspx.vb/.ascx.vb code-behind (and .designer.vb if present).\n",
    );
    s.push_str(
        "- Every user-facing string: update the .resx in EVERY language present, \
         not only the default - and use the resx FAMILY the ranked candidates \
         show (co-change evidence names the exact family, e.g. text vs label \
         vs control); do not add families on principle.\n",
    );
    s.push_str(
        "- UI look/feel changes: check whether the surface pulls page-specific \
         stylesheets (map.css-style files) - cross-cutting UI work in this \
         codebase frequently ships CSS alongside the TS/bundles.\n",
    );
    s.push_str("- Every schema / setting / column change: include the SQL migration.\n");
    s.push_str("- Every .ts that compiles into a committed bundle: update the bundle.\n");
    if temporal_section.is_some() {
        s.push_str(
            "- Analytics-over-time ask (time-to-X/aging/history): check the domain \
             entity's log/history tables above — computing durations needs the \
             status-change history, and teams typically expose it.\n",
        );
    }
    if sibling_section.is_some() {
        s.push_str(
            "- Symmetric pair(s) above: apply the change to BOTH sides or explicitly \
             state their interaction (mutually exclusive / mirrored / independent).\n",
        );
    }
    // Arm-B run 3 autopsy (PR1967): given a bare bug title, the agent
    // committed to its FIRST plausible root cause, then dismissed every
    // other candidate relative to that theory — while the real fix
    // spanned all 7 sibling call sites of the resource-selector family.
    // Thin bug reports get an explicit anti-confirmation-loop guard.
    let story_lower = story.to_lowercase();
    let bugish = [
        "bug", "fix", "can't", "cant ", "cannot", "doesn't", "does not", "fails", "broken",
        "error", "issue", "wrong",
    ]
    .iter()
    .any(|t| story_lower.contains(t));
    if bugish && story.chars().count() < 400 {
        s.push_str(
            "- THIN BUG REPORT (short story, no acceptance criteria): do NOT commit \
             to the first plausible root cause. Enumerate ALL plausible causes across \
             the candidate files BEFORE editing; bugs reported this thinly are usually \
             a defect CLASS, not a single site — after locating the culprit function, \
             list its callers and sibling call sites (the same pattern invoked \
             elsewhere) and check EVERY one for the same defect. Record why each \
             sibling does or does not need the fix.\n",
        );
    }
    // Arm-B run 2 autopsy (PR1933): the agent read the soft version of
    // this line, reasoned "unconditional UX improvement", and skipped the
    // setting — the PO gated it. When the corpus shows a real house
    // preference, state it as the DEFAULT, not a question.
    match setting_prior {
        Some((matched, scanned)) if matched * 10 >= scanned * 3 => {
            s.push_str(&format!(
                "- Story changes DEFAULT behaviour (UI or otherwise)? HOUSE PRIOR \
                 (corpus-mined): {matched} of {scanned} sampled merged PRs shipped \
                 system-setting changes alongside their feature. This team gates \
                 behaviour changes behind settings — DEFAULT TO ADDING ONE (settings \
                 store entry + localized descriptions in EVERY language resx + the \
                 settings SQL script) unless the story explicitly says the change is \
                 unconditional.\n\n"
            ));
        }
        _ => {
            s.push_str(
                "- Story changes DEFAULT behaviour (UI or otherwise)? Decide EXPLICITLY \
                 whether this team's convention calls for a NEW SETTING to gate it - \
                 mature codebases frequently ship behaviour changes as configurable \
                 toggles (settings store + admin UI + default), and stories rarely say \
                 so. Check how similar merged work did it (the exemplars below).\n",
            );
        }
    }
    s.push_str(
        "- Symptom names a user ACTION ('when removing/adding/clicking X')? Then the \
         fix must be VISIBLE AT THAT MOMENT - a fix that corrects the state only on \
         the next load/refresh/reopen does NOT close the report, even when it is the \
         cleaner root-cause fix (a live A/B: the correct-on-next-open server fix lost \
         to the team's dims-immediately client fix). State the feedback timing your \
         fix delivers.\n\n",
    );
    s.push_str("## Candidate files (grouped by layer — order within a group is NOT priority)\n");

    let _ = LAYERS; // layers now come from the shared model (CHANGE_SET_LAYERS)
    let (rows, omissions) = change_set_rows(prov);
    let mut current_layer: Option<usize> = None;
    for r in rows.iter().filter(|r| !r.omitted) {
        if current_layer != Some(r.layer_index) {
            current_layer = Some(r.layer_index);
            s.push_str(&format!("\n**{}:**\n", r.layer));
        }
        let hist = if historical.contains(&r.path) {
            "  (historical path — not in the current index)"
        } else {
            ""
        };
        s.push_str(&format!(
            "- `{}`  [{}]{hist}\n",
            r.path,
            r.signals.join("|")
        ));
    }
    if !omissions.is_empty() {
        s.push_str(&format!(
            "\n_{} weak-signal candidate(s) omitted by the per-layer tail cap \
             ({CHANGE_SET_TAIL_CAP}); listed under `omissions` in output_json._\n",
            omissions.len()
        ));
    }
    (s, omissions)
}

impl Engram {
    /// ONE call: the ranked, co-change-confirmed, family-aware change set for a
    /// user story. The pilot-validated recipe ported into Engram — concept
    /// footprint + git co-change, co-change/history ranked first, vendor noise
    /// filtered. Generic; no per-repo hardcoding.
    /// With `pat_token`, auto-fetches a referenced ADO work item for input
    /// parity (see `extract_work_item_id` / `fetch_ado_work_item`).
    pub async fn handle_get_change_set(
        &self,
        req: crate::models::GetChangeSetRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        if req.story.trim().is_empty() {
            return Err(McpError::invalid_params("story must not be empty", None));
        }
        // Input parity (arm-B run 3 vs 4: F1 22 -> 71 from this alone):
        // merge the full work-item text into the story at the front door so
        // EVERY downstream consumer — concept extraction, footprint
        // searches, the temporal/thin-bug triggers, scaffold detection, and
        // the rendered brief — sees what the developers actually received.
        let mut req = req;
        // Auto-fetch: story references a work-item id, a PAT is available
        // (per-call, or the server's own ADO_PAT env — a live agent never
        // holds credentials, the server host does), and refresh_corpora
        // saved the org/project coordinates. Silent degrade on any
        // failure — the dossier still builds from the story alone.
        if req.work_item_text.is_none()
            && let Some(wi_id) = extract_work_item_id(&req.story)
            && let Some(pat) = resolve_ado_pat(req.pat_token.take())
        {
            let reg = self.state.registry.clone();
            let pid = req.project_id.clone();
            let coords = tokio::task::spawn_blocking(move || {
                let org = reg.get_meta(&pid, "ado_org").ok().flatten();
                let project = reg.get_meta(&pid, "ado_project").ok().flatten();
                org.zip(project).or_else(|| {
                    // Zero-config fallback: an Azure DevOps remote URL
                    // already names the org/project, so the auto-fetch
                    // works on any ADO-backed repo even when no
                    // refresh_corpora stage-4 run ever persisted coords.
                    let dir = reg.get_project(&pid).ok().flatten()?.directory;
                    let url = git_remote_origin_url(std::path::Path::new(&dir))?;
                    ado_coords_from_remote_url(&url)
                })
            })
            .await
            .ok()
            .flatten();
            if let Some((org, project)) = coords {
                req.work_item_text = fetch_ado_work_item(&org, &project, wi_id, &pat).await;
            }
        }
        if let Some(wi) = req.work_item_text.take() {
            let wi = wi.trim();
            if !wi.is_empty() {
                req.story = format!("{}\n\n## Work item (full text)\n{}", req.story.trim(), wi);
            }
        }
        // Indexed file paths, loaded ONCE for this call: concept resolution
        // here; canonical paths and family expansion further down.
        let index_paths: Vec<String> = self
            .state
            .graph
            .list_file_node_metadata(&req.project_id)
            .map(|m| {
                m.iter()
                    .map(|(rp, _)| rp.as_str().replace('\\', "/"))
                    .collect()
            })
            .unwrap_or_default();
        // Entity candidates are always RESOLVED and REPORTED; they only
        // drive retrieval when the caller opts in (see the request doc).
        let concept_candidates: Vec<String> = {
            let cands = extract_story_concept_candidates(&story_for_concepts(&req.story));
            resolve_story_concepts(&cands, &index_paths, 6)
        };
        // External audit 2026-08-29 P0-3: an explicit gloss retrieves by DEFAULT.
        let gloss_terms = extract_story_gloss_concepts(&story_for_concepts(&req.story));
        let gloss_concepts: Vec<String> = gloss_derived(&gloss_terms, &concept_candidates)
            .into_iter()
            .cloned()
            .collect();
        // External audit 2026-08-29 row 1: the project's own .resx pairs translate
        // the story's English domain phrases into the Swedish code terms.
        let (lexicon_hits, lexicon_concepts) = {
            let dir = self
                .state
                .registry
                .get_project(&req.project_id)
                .ok()
                .flatten()
                .map(|r| std::path::PathBuf::from(r.directory));
            match dir {
                Some(d) => crate::services::lexicon::story_lexicon_concepts(
                    &self.state,
                    &req.project_id,
                    &d,
                    &story_for_concepts(&req.story),
                ),
                None => (Vec::new(), Vec::new()),
            }
        };
        let mut concepts: Vec<String> = match &req.concepts {
            Some(c) if !c.is_empty() => c.iter().take(3).cloned().collect(),
            _ if req.expand_concepts => concept_candidates.clone(),
            _ => {
                let mut base = extract_story_concepts(&story_for_concepts(&req.story));
                for g in gloss_concepts.iter().chain(lexicon_concepts.iter()) {
                    if !base.contains(g) {
                        base.push(g.clone());
                    }
                }
                base
            }
        };

        // Row-1 audit: every candidate carries WHY it is here and every arm
        // reports what it delivered (never a silent `if let Ok`).
        let mut why: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut cov = ChangeSetCoverage::default();
        let t_all = std::time::Instant::now();
        cov.concept_candidates = concept_candidates.clone();
        cov.gloss_concepts = gloss_concepts.clone();
        cov.lexicon_concepts = lexicon_hits
            .iter()
            .map(|h| format!("{} → {}", h.en, h.sv))
            .collect();
        // KB language bridge: the team's wiki/docs corpus (memory_bank
        // sections) frequently names the same feature in BOTH the story's
        // language and the code's (English story "resource planning" vs
        // Swedish identifiers "resurs*"). Mine the top sections matching
        // the story for identifier-ish tokens that (a) recur across
        // sections, (b) are NOT already reachable from the story's own
        // concepts, and (c) actually exist in the code graph - and add up
        // to TWO of them as extra concepts. Generic: no language tables.
        let t_kb = std::time::Instant::now();
        let kb_runtime = self.ensure_project_runtime(&req.project_id).await;
        if let Err(e) = &kb_runtime {
            cov.kb_bridge = ArmCoverage::failed(
                format!("search runtime unavailable: {e}"),
                t_kb.elapsed().as_millis(),
            );
        }
        if let Ok(ps) = kb_runtime {
            let q = engram_index::HybridQuery {
                project_id: req.project_id.clone(),
                namespace: engram_core::namespaces::NAMESPACE_MEMORY_BANK.into(),
                generation: 0,
                text: story_for_concepts(&req.story),
                top_k: 3,
                fts_mode: "loose".into(),
                include_path_prefixes: None,
                exclude_path_prefixes: None,
                language_filters: None,
                author_filter: None,
                date_after: None,
                date_before: None,
                use_mmr: false,
            };
            let engine = ps.search.clone();
            let hits = match tokio::task::spawn_blocking(move || engine.lexical_search(&q)).await {
                Ok(Ok(h)) => h,
                Ok(Err(e)) => {
                    cov.kb_bridge = ArmCoverage::failed(
                        format!("memory-bank search failed: {e}"),
                        t_kb.elapsed().as_millis(),
                    );
                    Vec::new()
                }
                Err(e) => {
                    cov.kb_bridge = ArmCoverage::failed(
                        format!("memory-bank search task failed: {e}"),
                        t_kb.elapsed().as_millis(),
                    );
                    Vec::new()
                }
            };
            let mut counts: std::collections::HashMap<String, usize> = Default::default();
            for h in hits.iter().take(3) {
                let Ok(Some((_, _, content, _, _))) = ps.search.get_doc_by_doc_id(
                    &req.project_id,
                    engram_core::namespaces::NAMESPACE_MEMORY_BANK,
                    0,
                    &h.doc_id,
                ) else {
                    continue;
                };
                let mut seen_in_doc: std::collections::HashSet<String> = Default::default();
                for tok in content
                    .split(|c: char| !c.is_alphanumeric())
                    .filter(|t| t.len() >= 5 && t.chars().all(|c| c.is_alphabetic()))
                {
                    let t = tok.to_lowercase();
                    if seen_in_doc.insert(t.clone()) {
                        *counts.entry(t).or_default() += 1;
                    }
                }
            }
            let covered = concepts.join(" ").to_lowercase();
            let mut cands: Vec<(usize, String)> = counts
                .into_iter()
                .filter(|(t, n)| *n >= 2 && !covered.contains(t.as_str()))
                .map(|(t, n)| (n, t))
                .collect();
            cands.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
            let graph_b = self.state.graph.clone();
            let pid_b = req.project_id.clone();
            let picked = tokio::task::spawn_blocking(move || {
                let mut picked: Vec<String> = Vec::new();
                for (_, t) in cands.into_iter().take(24) {
                    if picked.len() >= 2 {
                        break;
                    }
                    let in_code = graph_b
                        .query_nodes(&pid_b, None, Some(&t), None, 3)
                        .map(|v| !v.is_empty())
                        .unwrap_or(false);
                    if in_code {
                        picked.push(t);
                    }
                }
                picked
            })
            .await
            .unwrap_or_default();
            let mut added = 0usize;
            for t in picked {
                if concepts.len() < 8 {
                    concepts.push(t);
                    added += 1;
                }
            }
            if cov.kb_bridge.status.is_empty() {
                cov.kb_bridge = ArmCoverage::complete(added, t_kb.elapsed().as_millis());
            }
        }
        let mut prov: BTreeMap<String, BTreeSet<&'static str>> = BTreeMap::new();
        let mut seed_order: Vec<String> = Vec::new(); // concept hits in relevance order

        // Concept arm — typed footprint of each domain concept.
        let t_concept = std::time::Instant::now();
        let mut concept_hits = 0usize;
        let mut concept_failures: Vec<String> = Vec::new();
        for c in &concepts {
            match self
                .handle_get_concept_footprint(crate::models::GetConceptFootprintRequest {
                    project_id: req.project_id.clone(),
                    concept: c.clone(),
                    max_per_group: 12,
                })
                .await
            {
                Ok(r) => {
                    if let Some(t) = r.content.first().and_then(|x| x.as_text()) {
                        for p in change_set_paths(&t.text) {
                            if !engram_core::is_vendor_path(&p) {
                                if !prov.contains_key(&p) {
                                    seed_order.push(p.clone());
                                }
                                concept_hits += 1;
                                let from_gloss = gloss_concepts.contains(c);
                                let from_lexicon = lexicon_concepts.contains(c);
                                why.entry(p.clone()).or_default().push(if from_gloss {
                                    format!("matches the story's explicit gloss '{c}'")
                                } else if from_lexicon {
                                    format!("matches '{c}' — the project's .resx translation of the story's English term")
                                } else {
                                    format!("name/content matches concept '{c}'")
                                });
                                let e = prov.entry(p).or_default();
                                e.insert("concept");
                                if from_gloss {
                                    e.insert("gloss");
                                }
                                if from_lexicon {
                                    e.insert("lexicon");
                                }
                            }
                        }
                    }
                }
                Err(e) => concept_failures.push(format!("'{c}': {e}")),
            }
        }
        cov.concept = if concept_failures.is_empty() {
            ArmCoverage::complete(concept_hits, t_concept.elapsed().as_millis())
        } else {
            ArmCoverage::failed(
                format!("footprint failed for {}", concept_failures.join("; ")),
                t_concept.elapsed().as_millis(),
            )
        };

        // History arm — commit-message search surfaces the files of past similar
        // changes (the universal co-change signal; carries stories whose real
        // files share no concept keyword). Golden tier.
        let t_hist = std::time::Instant::now();
        match self
            .handle_search_history(crate::models::SearchHistoryRequest {
                project_id: req.project_id.clone(),
                query: req.story.clone(),
                file_filter: None,
                exclude_paths: None,
                author_filter: None,
                date_after: None,
                date_before: None,
                limit: 12,
                fts_mode: crate::models::FtsMode::Loose,
                use_mmr: false,
                max_content_chars: 0,
            })
            .await
        {
            Ok(r) => {
                let mut n = 0usize;
                if let Some(t) = r.content.first().and_then(|x| x.as_text()) {
                    for p in change_set_paths(&t.text) {
                        if !engram_core::is_vendor_path(&p) {
                            if !prov.contains_key(&p) {
                                seed_order.push(p.clone());
                            }
                            n += 1;
                            why.entry(p.clone()).or_default().push(
                                "past commits matching the story touched it (history search)"
                                    .into(),
                            );
                            prov.entry(p).or_default().insert("history");
                        }
                    }
                }
                cov.history = ArmCoverage::complete(n, t_hist.elapsed().as_millis());
            }
            Err(e) => {
                cov.history = ArmCoverage::failed(
                    format!("history search failed: {e}"),
                    t_hist.elapsed().as_millis(),
                );
            }
        }

        // Co-change arm — confirm/expand real companions. HISTORY hits rank
        // first (the strongest co-change anchors), then concept hits in
        // relevance order, so history-carried stories aren't crowded out.
        //
        // The two co-change tools have opposite economics, so they get
        // different seed widths:
        //  • find_similar_changes RE-WALKS GIT (slow) → seed it NARROW (top 12).
        //  • detect_incomplete_changes is a graph-neighbour lookup (cheap) and
        //    SELF-FILTERING: a file with no strong (weight≥5) co-change history
        //    returns nothing. So seed it BROAD. A central file — e.g. a settings
        //    store — can rank deep in concept order yet be the ONLY anchor that
        //    pulls in the tight companion family the story needs (the full .resx
        //    language set + the SQL seed migration). Tangential anchors cost one
        //    lookup and contribute nothing; the tool's internal weight-sort and
        //    cap bound the output regardless of how wide we seed. Generic: any
        //    framework where a hub file co-changes with a consistent satellite set.
        let mut ranked: Vec<String> = prov
            .iter()
            .filter(|(_, s)| s.contains("history"))
            .map(|(p, _)| p.clone())
            .collect();
        for p in &seed_order {
            if !ranked.contains(p) {
                ranked.push(p.clone());
            }
        }
        // ONE full node scan for every pass of this call (audit D7).
        let snapshot: NodeSnapshot = {
            let graph = self.state.graph.clone();
            let pid = req.project_id.clone();
            tokio::task::spawn_blocking(move || {
                std::sync::Arc::new(
                    graph
                        .query_nodes(&pid, None, None, None, crate::handlers::NODE_SCAN_LIMIT)
                        .unwrap_or_default(),
                )
            })
            .await
            .unwrap_or_default()
        };
        cov.node_scans = 1;
        cov.stages
            .insert("node_scan".into(), t_all.elapsed().as_millis());
        if ranked.is_empty() {
            cov.cochange = ArmCoverage::not_run("no concept/history seeds to anchor on");
        }
        if !ranked.is_empty() {
            // Resx are family LEAVES, not co-change ANCHORS: a resource file
            // co-changes only with its own language siblings, never with the
            // diverse set a story spans. Indexing resx (so the family expansion
            // can backfill the language set) floods concept matches, which can
            // shove the real code anchor — e.g. a settings store whose history
            // pulls the whole resx family — past the seed cap. Keep resx out of
            // the co-change seed so code anchors keep their slots; resx still
            // reach the change set via concept, co-change PARTNERS, and family.
            let anchor = |p: &&String| !p.ends_with(".resx");
            let fsc_seed: Vec<String> = ranked.iter().filter(anchor).take(12).cloned().collect();
            let dic_seed: Vec<String> = ranked.iter().filter(anchor).take(40).cloned().collect();
            let t_cc = std::time::Instant::now();
            let mut texts: Vec<String> = Vec::new();
            let mut cc_notes: Vec<String> = Vec::new();
            let mut cc_partial = false;
            match self
                .handle_find_similar_changes(crate::models::FindSimilarChangesRequest {
                    project_id: req.project_id.clone(),
                    files: fsc_seed,
                    max_commits: 800,
                    top: 8,
                })
                .await
            {
                Ok(r) => {
                    if let Some(t) = r.content.first().and_then(|x| x.as_text()) {
                        // The walk's own completeness marker must reach the
                        // caller instead of dying at this tool boundary.
                        if let Some(line) = t.text.lines().find(|l| l.contains("PARTIAL")) {
                            cc_partial = true;
                            cc_notes.push(line.trim().to_string());
                        }
                        texts.push(t.text.clone());
                    }
                }
                Err(e) => cc_notes.push(format!("find_similar_changes failed: {e}")),
            }
            match self
                .detect_incomplete_changes_with(
                    crate::models::DetectIncompleteChangesRequest {
                        project_id: req.project_id.clone(),
                        edited_files: dic_seed,
                        max_partners: 12,
                    },
                    Some(snapshot.clone()),
                )
                .await
            {
                Ok(r) => {
                    if let Some(t) = r.content.first().and_then(|x| x.as_text()) {
                        texts.push(t.text.clone());
                    }
                }
                Err(e) => cc_notes.push(format!("detect_incomplete_changes failed: {e}")),
            }
            let mut n = 0usize;
            for text in texts {
                for p in change_set_paths(&text) {
                    if !engram_core::is_vendor_path(&p) {
                        n += 1;
                        why.entry(p.clone())
                            .or_default()
                            .push("co-changed with the seed files in merged history".into());
                        prov.entry(p).or_default().insert("cochange");
                    }
                }
            }
            let ms = t_cc.elapsed().as_millis();
            cov.cochange = if cc_notes.iter().any(|x| x.contains("failed")) && !cc_partial {
                ArmCoverage::failed(cc_notes.join("; "), ms)
            } else if cc_partial {
                ArmCoverage::truncated(n, ms, cc_notes.join("; "))
            } else {
                ArmCoverage::complete(n, ms)
            };
        }

        // Semantic arm — embedding search reaches files the LEXICAL signals miss:
        // a new architectural layer (e.g. an api-v2 controller) with sparse git
        // history is invisible to concept/co-change/history, but its meaning
        // ("update RoQ invoice status from the API") still matches the story
        // vector. Tagged "vector"; ranked LOW alone (semantic hits are noisier),
        // so it fills the capped tail to reach those files without displacing the
        // corroborated ones. MMR for file diversity. Generic; no per-repo logic.
        let t_vec = std::time::Instant::now();
        let vector_result = self
            .handle_vector_search(crate::models::VectorSearchRequest {
                project_id: req.project_id.clone(),
                query: req.story.clone(),
                namespace: "memory".to_string(),
                top_k: 40,
                use_mmr: true,
                include_path_prefixes: None,
                exclude_path_prefixes: None,
                language_filters: None,
                include_content: false,
                max_content_chars: 0,
            })
            .await;
        if let Err(e) = &vector_result {
            cov.vector = ArmCoverage::failed(
                format!("vector search unavailable: {e}"),
                t_vec.elapsed().as_millis(),
            );
        }
        if let Ok(r) = vector_result
            && let Some(t) = r.content.first().and_then(|x| x.as_text())
        {
            let mut n = 0usize;
            // change_set_paths preserves the similarity order of the results.
            // The TOP-12 distinct hits are the cross-LANGUAGE bridge — an English
            // story matching Swedish/internal code identifiers that concept and
            // co-change cannot reach — so tag them "vtop" to ALWAYS survive the
            // render tail cap (bounded → never floods). Ranks 13+ stay plain
            // "vector" (normal tail behaviour, so no regression where the layer
            // had room for them).
            for (i, p) in change_set_paths(&t.text)
                .into_iter()
                .filter(|p| !engram_core::is_vendor_path(p))
                .enumerate()
            {
                n += 1;
                why.entry(p.clone())
                    .or_default()
                    .push(format!("semantic match to the story (rank {})", i + 1));
                prov.entry(p)
                    .or_default()
                    .insert(if i < 12 { "vtop" } else { "vector" });
            }
            cov.vector = ArmCoverage::complete(n, t_vec.elapsed().as_millis());
        }

        // TS -> committed JS/CSS BUNDLE via co-change. A .ts usually ships with a
        // compiled bundle, but MANY .ts often compile into ONE module bundle with
        // a DIFFERENT basename (map.ts/iomarker.ts/… -> map.js), so the 1:1
        // transpile-pair expansion misses it. The bundle IS among the .ts's
        // strongest co-change partners, but the broad 40-anchor co-change seed
        // above buries those moderate-weight links under the partner truncation.
        // Re-run the cheap neighbour lookup seeded with ONLY the .ts anchors so
        // each source's committed bundle survives, and keep just the .js/.css
        // partners (the bundles). Generic — any framework that commits compiled
        // front-end bundles; no per-repo names.
        // Generalised to the whole PRESENTATION layer: markup, code's compiled
        // bundle, and stylesheet all ship together but rarely share a basename
        // (map.ts/iomarker.ts -> map.js; map.js <-> map.aspx/index.vbhtml/map.css;
        // marker_edit.aspx <-> marker.edit.js), so 1:1 pairing misses them and the
        // broad co-change seed buries the moderate-weight links. Seed the cheap
        // neighbour lookup with ONLY the presentation anchors and keep their
        // presentation-type partners. Hub-trim inside the tool bounds it.
        const PRESENTATION: &[&str] = &[
            ".ts", ".tsx", ".js", ".jsx", ".aspx", ".ascx", ".master", ".vbhtml", ".cshtml", ".css",
        ];
        let pres_anchors: Vec<String> = prov
            .keys()
            .filter(|p| {
                let pl = p.to_lowercase();
                PRESENTATION.iter().any(|e| pl.ends_with(e))
            })
            .cloned()
            .collect();
        if !pres_anchors.is_empty()
            && let Ok(r) = self
                .detect_incomplete_changes_with(
                    crate::models::DetectIncompleteChangesRequest {
                        project_id: req.project_id.clone(),
                        // Wider than the per-file default: this seeds the WHOLE
                        // presentation anchor set in one call, so a small cap would let
                        // strong anchors' partners crowd out a specific bundle (e.g.
                        // roqQtyManager.js, weight ~8) under truncation.
                        edited_files: pres_anchors,
                        max_partners: 25,
                    },
                    Some(snapshot.clone()),
                )
                .await
            && let Some(t) = r.content.first().and_then(|x| x.as_text())
        {
            for p in change_set_paths(&t.text) {
                let pl = p.to_lowercase();
                if PRESENTATION.iter().any(|e| pl.ends_with(e)) && !engram_core::is_vendor_path(&p)
                {
                    why.entry(p.clone()).or_default().push(
                        "presentation-layer co-change partner (bundle / markup / stylesheet)"
                            .into(),
                    );
                    prov.entry(p).or_default().insert("cochange");
                }
            }
        }

        // Family expansion — add deterministic .NET companions that EXIST in the
        // index: code-behind/designer of a page, and the full .resx language set.
        // Generic framework patterns, not per-repo. Match prefix-insensitively
        // (the "Site/" web-root prefix is stripped on both sides).
        if let Ok(meta) = self.state.graph.list_file_node_metadata(&req.project_id) {
            let strip = |p: &str| -> String {
                let p = p.replace('\\', "/").to_lowercase();
                p.strip_prefix("site/").unwrap_or(&p).to_string()
            };
            let index: Vec<String> = meta.iter().map(|(rp, _)| strip(rp.as_str())).collect();
            let index_set: HashSet<&String> = index.iter().collect();
            let mut fam: Vec<(String, BTreeSet<&'static str>)> = Vec::new();
            for (p, sigs) in &prov {
                let ps = strip(p);
                if ps.ends_with(".aspx") || ps.ends_with(".ascx") {
                    for ext in [".vb", ".cs", ".designer.vb", ".designer.cs"] {
                        let sib = format!("{ps}{ext}");
                        if index_set.contains(&sib) {
                            why.entry(sib.clone())
                                .or_default()
                                .push(format!("code-behind/designer companion of {p}"));
                            fam.push((sib, sigs.clone()));
                        }
                    }
                }
                if ps.ends_with(".resx")
                    && let Some(slash) = ps.rfind('/')
                {
                    let dir = &ps[..slash + 1];
                    let stem = ps[slash + 1..].split('.').next().unwrap_or("");
                    if !stem.is_empty() {
                        let want = format!("{dir}{stem}");
                        for f in &index {
                            if f.starts_with(&want) && f.ends_with(".resx") {
                                // Tag "family": a localized resx set is atomic — if
                                // one language is in the set, ALL must be (the
                                // completeness rule). Tagging lets render exempt the
                                // language siblings from the per-layer tail cap, so a
                                // seeded family is never shown half-complete.
                                let mut fs = sigs.clone();
                                fs.insert("family");
                                why.entry(f.clone())
                                    .or_default()
                                    .push(format!("localized .resx sibling of {p} (atomic set)"));
                                fam.push((f.clone(), fs));
                            }
                        }
                    }
                }
                // TypeScript source <-> its committed compiled JS bundle. The
                // committed bundle MUST change with its source (a recurring recall
                // miss: editing a .ts but forgetting the shipped .js). Only add a
                // partner that EXISTS in the index.
                for c in transpile_pair_candidates(&ps) {
                    if index_set.contains(&c) {
                        why.entry(c.clone())
                            .or_default()
                            .push(format!("compiled bundle / source pair of {p}"));
                        fam.push((c, sigs.clone()));
                    }
                }
                // Interface <-> implementation (.NET IService convention).
                for c in interface_pair_candidates(&ps) {
                    if index_set.contains(&c) {
                        why.entry(c.clone())
                            .or_default()
                            .push(format!("interface/implementation pair of {p}"));
                        fam.push((c, sigs.clone()));
                    }
                }
            }
            // API-spec contract documents: set-level rule — any API-layer
            // code candidate pulls the OpenAPI/Swagger docs that exist in
            // the index. Tagged "family" (asserted-contract companion,
            // exempt from the tail cap like the resx language sets).
            if prov.keys().any(|p| is_api_code_path(&strip(p))) {
                for f in api_spec_docs(&index) {
                    why.entry(f.clone())
                        .or_default()
                        .push("API contract document (API-layer code is in the set)".into());
                    fam.push((f, BTreeSet::from(["family"])));
                }
            }
            cov.family = ArmCoverage::complete(fam.len(), 0);
            for (k, v) in fam {
                prov.entry(k).or_default().extend(v);
            }
        } else {
            cov.family = ArmCoverage::failed("file index unavailable".into(), 0);
        }

        // Collapse path-prefix variants. The graph stores some files under two
        // node spellings — index_project keeps the web-root prefix
        // (Site/App_Code/…) while index_git_history drops it (App_Code/…) — so
        // one file can appear twice in prov with its signal tags split across
        // the two spellings, padding the change set and weakening per-file
        // corroboration. Merge any path that is a ONE-SEGMENT trailing suffix of
        // a longer path (exactly the web-root-prefix pattern) into the longer,
        // fully-qualified one, unioning tags (which can also lift the survivor's
        // tier as it gains a second signal). Pure within-set rule — robust even
        // when BOTH spellings exist as graph nodes; generic, no web-root name.
        {
            let mut keys: Vec<String> = prov.keys().cloned().collect();
            keys.sort_by(|a, b| b.len().cmp(&a.len())); // longest first
            let mut kept: Vec<String> = Vec::new();
            let mut remap: HashMap<String, String> = HashMap::new();
            for k in keys {
                let k_segs = k.matches('/').count();
                if let Some(longer) = kept
                    .iter()
                    .find(|a| a.matches('/').count() == k_segs + 1 && a.ends_with(&format!("/{k}")))
                {
                    remap.insert(k.clone(), longer.clone());
                } else {
                    kept.push(k);
                }
            }
            if !remap.is_empty() {
                let mut merged: BTreeMap<String, BTreeSet<&'static str>> = BTreeMap::new();
                let mut merged_why: BTreeMap<String, Vec<String>> = BTreeMap::new();
                for (p, sigs) in std::mem::take(&mut prov) {
                    let key = remap.get(&p).cloned().unwrap_or(p.clone());
                    merged.entry(key.clone()).or_default().extend(sigs);
                    if let Some(w) = why.remove(&p) {
                        merged_why.entry(key).or_default().extend(w);
                    }
                }
                prov = merged;
                why = merged_why;
            }
        }

        // Canonical indexed paths (row-1 audit D9): every candidate is the
        // path the INDEX knows (case and `Site/` prefix restored); a path
        // that no longer exists in the index is kept but labelled historical.
        let mut historical: BTreeSet<String> = BTreeSet::new();
        if let Ok(meta) = self.state.graph.list_file_node_metadata(&req.project_id) {
            let norm = |p: &str| -> String {
                let p = p.replace('\\', "/").to_lowercase();
                p.strip_prefix("site/").unwrap_or(&p).to_string()
            };
            let index_map: HashMap<String, String> = meta
                .iter()
                .map(|(rp, _)| (norm(rp.as_str()), rp.as_str().replace('\\', "/")))
                .collect();
            let mut canon: BTreeMap<String, BTreeSet<&'static str>> = BTreeMap::new();
            let mut canon_why: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for (p, sigs) in std::mem::take(&mut prov) {
                let key = match index_map.get(&norm(&p)) {
                    Some(c) => c.clone(),
                    None => {
                        historical.insert(p.clone());
                        p.clone()
                    }
                };
                canon.entry(key.clone()).or_default().extend(sigs);
                if let Some(w) = why.remove(&p) {
                    canon_why.entry(key).or_default().extend(w);
                }
            }
            prov = canon;
            why = canon_why;
        }

        // Temporal-analytics expansion: a story asking for analytics OVER TIME
        // ("time to approve", aging, trends) needs the domain entity's
        // STATUS-CHANGE HISTORY — which lives in log/history tables the
        // concept/co-change arms miss because the story names the entity, not
        // its log table (a live A/B: the dossier missed the log table an
        // approved PR queried for exactly this ask). Surface the graph's
        // log/history/audit tables plus their accessor functions so the plan
        // decides explicitly. Generic: name-shape scan, no per-repo names.
        let temporal_section: Option<String> = if story_asks_analytics_over_time(&req.story) {
            let graph = self.state.graph.clone();
            let pid_t = req.project_id.clone();
            let cand_files: Vec<String> = prov.keys().cloned().collect();
            tokio::task::spawn_blocking(move || {
                let tables = graph
                    .query_nodes(
                        &pid_t,
                        Some("db_table"),
                        None,
                        None,
                        crate::handlers::NODE_SCAN_LIMIT,
                    )
                    .unwrap_or_default();
                let cand_norm: Vec<String> = cand_files
                    .iter()
                    .map(|f| f.replace('\\', "/").to_lowercase())
                    .collect();
                let cand_dirs: HashSet<String> = cand_norm
                    .iter()
                    .filter_map(|f| f.rfind('/').map(|i| f[..i].to_string()))
                    .collect();
                let cand_words: HashSet<String> = path_token_bag(&cand_norm)
                    .into_iter()
                    .filter(|t| t.starts_with("w:"))
                    .collect();
                // (related-to-candidates, accessor count, table, accessor rows)
                let mut rows: Vec<(bool, usize, String, Vec<String>)> = Vec::new();
                for t in tables.iter().filter(|t| is_history_log_table_name(&t.name)) {
                    let mut incoming = graph
                        .find_incoming_edges_with_kind(&pid_t, None, &t.node_id, 50)
                        .unwrap_or_default();
                    // LINQ-to-SQL split: code reads via the DataContext's
                    // Table(Of X) property, conventionally the PLURAL
                    // (io_pr_iom_logs), while the db_table NODE is the singular
                    // DDL name (io_pr_iom_log) — so the singular schema node has
                    // 0 accessors while the plural queries_table edges land on
                    // a target that has NO node at all. The adjacency is keyed
                    // by the edge target_id = NodeId::table(name), which we can
                    // COMPUTE without a node existing — so query the plural
                    // target's incoming edges directly. Generic English-plural
                    // shapes, no per-repo names (confirmed live 2026-07-06).
                    if incoming.is_empty() {
                        let base = t.name.to_lowercase();
                        let mut cands = vec![format!("{base}s"), format!("{base}es")];
                        if let Some(stem) = base.strip_suffix('y') {
                            cands.push(format!("{stem}ies"));
                        }
                        for c in cands {
                            let nid = engram_core::ids::NodeId::table(&c).0;
                            let extra = graph
                                .find_incoming_edges_with_kind(&pid_t, None, &nid, 50)
                                .unwrap_or_default();
                            if !extra.is_empty() {
                                incoming = extra;
                                break;
                            }
                        }
                    }
                    let mut accessors: Vec<String> = Vec::new();
                    let mut related = false;
                    let mut total = 0usize;
                    for (src, _, _) in &incoming {
                        let Ok(Some(f)) = graph.get_node(&pid_t, src) else {
                            continue;
                        };
                        if f.node_type != "function" {
                            continue;
                        }
                        total += 1;
                        let fp = f.file_path.as_str().replace('\\', "/");
                        let fpl = fp.to_lowercase();
                        if !related {
                            // "Related" = an accessor file shares a directory
                            // prefix or a basename word with the ranked set.
                            let dir_match = fpl
                                .rfind('/')
                                .map(|i| fpl[..i].to_string())
                                .is_some_and(|d| {
                                    cand_dirs
                                        .iter()
                                        .any(|c| c.starts_with(&d) || d.starts_with(c.as_str()))
                                });
                            let word_match = path_token_bag(std::slice::from_ref(&fpl))
                                .iter()
                                .any(|w| w.starts_with("w:") && cand_words.contains(w));
                            related = dir_match || word_match;
                        }
                        if accessors.len() < 3 {
                            accessors.push(format!("{} ({}:{})", f.name, fp, f.start_line));
                        }
                    }
                    rows.push((related, total, t.name.clone(), accessors));
                }
                if rows.is_empty() {
                    return None;
                }
                // Prefer tables whose accessors overlap the ranked candidates;
                // when NONE overlap, show all matches with a note rather than
                // guess (the overlap heuristic is a ranking aid, not an oracle).
                let any_related = rows.iter().any(|(r, ..)| *r);
                if any_related {
                    rows.retain(|(r, ..)| *r);
                }
                rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.2.cmp(&b.2)));
                let mut s = String::from(
                    "## History/log tables (analytics-over-time signal)\n\
                     This story asks for analytics OVER TIME (time-to-X / aging / \
                     history). Computing a duration needs the STATUS-CHANGE \
                     HISTORY, and this codebase keeps it in tables like these — \
                     decide explicitly which one holds this story's domain entity:\n",
                );
                if !any_related {
                    s.push_str(
                        "(no accessor file overlaps the ranked candidates — showing \
                         all log/history tables in the graph; verify domain \
                         relevance)\n",
                    );
                }
                for (_, total, name, accessors) in rows.iter().take(8) {
                    if accessors.is_empty() {
                        s.push_str(&format!(
                            "- `{name}` (no accessor functions in the graph)\n"
                        ));
                    } else {
                        s.push_str(&format!(
                            "- `{name}` — accessed by: {}{}\n",
                            accessors.join(", "),
                            if *total > accessors.len() {
                                format!(" (+{} more)", total - accessors.len())
                            } else {
                                String::new()
                            }
                        ));
                    }
                }
                s.push('\n');
                Some(s)
            })
            .await
            .ok()
            .flatten()
        } else {
            None
        };

        // Symmetric sibling controls/handlers: WebForms/WinForms pages often
        // contain PAIRED controls/handlers whose names differ by exactly one
        // token (Main/Sub, Left/Right, From/To, Start/End, Sender/Receiver);
        // a change to one usually requires deciding the interaction with the
        // other (a live case: a merged fix made two symmetric filter panels
        // mutually exclusive — the plan targeting one control under-specified
        // the twin). Scan control + function nodes in the top provisional
        // files and surface one-token-apart name pairs. Generic: name-shape
        // scan, no per-repo names.
        cov.stages
            .insert("arms_done".into(), t_all.elapsed().as_millis());
        let sibling_section: Option<String> = {
            // "Top" = corroboration tier, then MORE signals, restricted to
            // files that can hold control/function nodes (see the helper's
            // doc for the two live traps this avoids).
            let top_files = top_node_bearing_files(&prov, 10);
            let graph = self.state.graph.clone();
            let pid_s = req.project_id.clone();
            tokio::task::spawn_blocking(move || {
                const MAX_ROWS: usize = 12;
                let mut seen_pairs: HashSet<(String, String)> = HashSet::new();
                // Collect ALL pairs across ALL candidate files FIRST, then
                // rank by relevance tier and truncate — a lifecycle-only
                // file that ranks above the toggle-rich one must NOT eat the
                // whole budget in file order (live: PR1933 dossier put
                // CustomerInvoiceImporter Add/Clear/Commit noise above the
                // Show/Hide/Toggle Main/Sub pairs the story needed).
                let mut rows: Vec<(String, String, String)> = Vec::new();
                for f in &top_files {
                    let mut names: Vec<String> = Vec::new();
                    for nt in ["control", "function"] {
                        for n in graph
                            .query_nodes(&pid_s, Some(nt), None, Some(f), 500)
                            .unwrap_or_default()
                        {
                            names.push(n.name);
                        }
                    }
                    for (a, b) in symmetric_sibling_pairs(&names) {
                        // Two prov spellings of one file (web-root prefix
                        // variants; a page's markup vs code-behind matching
                        // the same nodes via the substring path filter) must
                        // not spend the row budget on duplicate pairs.
                        if seen_pairs.insert((a.clone(), b.clone())) {
                            rows.push((f.clone(), a, b));
                        }
                    }
                }
                if rows.is_empty() {
                    return None;
                }
                // Tier desc across files; stable within a tier.
                rows.sort_by(|x, y| {
                    symmetric_pair_tier(&y.1, &y.2).cmp(&symmetric_pair_tier(&x.1, &x.2))
                });
                rows.truncate(MAX_ROWS);
                let mut s = String::from(
                    "## Symmetric sibling controls/handlers\n\
                     These controls/handlers in the candidate files come in \
                     NAME-SYMMETRIC pairs (names differ by exactly one token — \
                     Main/Sub, Left/Right, From/To, Start/End). A change to one \
                     side of a pair usually requires deciding the twin's \
                     behavior:\n",
                );
                for (f, a, b) in &rows {
                    s.push_str(&format!("- `{a}` <-> `{b}` ({f})\n"));
                }
                s.push('\n');
                Some(s)
            })
            .await
            .ok()
            .flatten()
        };

        // House-prior mining: the team's revealed preference on setting-
        // gating, from the merged-PR corpus (>=2 "setting"-named files in
        // one PR's shipped list = a settings change: store + resx family
        // or resx + sql). Enumerates the MOST RECENT 60 PR docs directly —
        // search was the wrong transport (settings-PR text rarely contains
        // the literal word "setting"; it hides inside compound path tokens
        // like "systemsettings", which FTS won't split). Degrades to the
        // soft decision line when the corpus is absent or the sample thin.
        let setting_prior: Option<(usize, usize)> =
            if let Ok(ps) = self.ensure_project_runtime(&req.project_id).await {
                let search = ps.search.clone();
                let pid = req.project_id.clone();
                tokio::task::spawn_blocking(move || {
                    // Scoped to the history namespace in the QUERY. Listing
                    // the whole project materialised every doc's stored
                    // fields and then discarded all but the pr:* ones.
                    let mut pr_docs: Vec<(u64, String)> = search
                        .list_docs_in_namespace(&pid, engram_core::namespaces::NAMESPACE_HISTORY)
                        .ok()?
                        .into_iter()
                        .filter(|d| d.path.starts_with("pr:"))
                        .map(|d| {
                            let num = d.path[3..]
                                .split(|c: char| !c.is_ascii_digit())
                                .next()
                                .and_then(|s| s.parse::<u64>().ok())
                                .unwrap_or(0);
                            (num, d.doc_id)
                        })
                        .collect();
                    pr_docs.sort_by(|a, b| b.0.cmp(&a.0));
                    let mut scanned = 0usize;
                    let mut matched = 0usize;
                    for (_, doc_id) in pr_docs.into_iter().take(60) {
                        let Ok(Some((_, _, content, _, _))) = search.get_doc_by_doc_id(
                            &pid,
                            engram_core::namespaces::NAMESPACE_HISTORY,
                            0,
                            &doc_id,
                        ) else {
                            continue;
                        };
                        let files =
                            crate::services::pre_commit_review_service::gates::parse_pr_doc_files(
                                &content,
                            );
                        if files.is_empty() {
                            continue;
                        }
                        scanned += 1;
                        let n = files
                            .iter()
                            .filter(|f| {
                                let fl = f.to_lowercase();
                                fl.rsplit('/').next().unwrap_or(&fl).contains("setting")
                            })
                            .count();
                        if n >= 2 {
                            matched += 1;
                        }
                    }
                    if scanned >= 10 {
                        Some((matched, scanned))
                    } else {
                        None
                    }
                })
                .await
                .ok()
                .flatten()
            } else {
                None
            };

        cov.stages
            .insert("before_render".into(), t_all.elapsed().as_millis());
        let (mut out, omissions) = render_change_set(
            req.story.trim(),
            &concepts,
            &prov,
            temporal_section.as_deref(),
            sibling_section.as_deref(),
            setting_prior,
            &historical,
        );
        cov.stages
            .insert("render".into(), t_all.elapsed().as_millis());
        cov.wall_ms = t_all.elapsed().as_millis();
        out.push_str(&render_change_set_coverage(&cov, omissions.len()));

        let graph = self.state.graph.clone();
        let pid_g = req.project_id.clone();
        // Same trap as the sibling scan: a plain `prov.keys().take(15)` is
        // BTreeMap-alphabetical, so the App_GlobalResources/*.resx family
        // (tier-0 via inherited co-change) ate the whole scan budget and
        // the gated code files (role.vb, *.aspx.vb) were never queried —
        // the section silently vanished. Pick node-bearing code files,
        // strongest corroboration first.
        let top_files = top_node_bearing_files(&prov, 15);
        let (gates, helper_files, gate_def_files) = tokio::task::spawn_blocking(move || {
            let mut gates: std::collections::BTreeMap<String, usize> = Default::default();
            for f in &top_files {
                for n in graph
                    .query_nodes(&pid_g, None, None, Some(f), 500)
                    .unwrap_or_default()
                {
                    let Some(meta) = n.metadata.as_ref() else {
                        continue;
                    };
                    for key in ["permission_checks", "guard_roles"] {
                        if let Some(v) = meta.get(key).and_then(|v| v.as_str()) {
                            for g in v.split(';').filter(|g| !g.trim().is_empty()) {
                                *gates.entry(g.trim().to_string()).or_default() += 1;
                            }
                        }
                    }
                }
            }
            // House auth-helper convention: where Can* permission
            // helpers are DEFINED. Two arm-B autopsies missed the same
            // class (PR1890 role.vb, PR1913 aspnetUsers.vb): the team
            // routes new permission surface through its user/role
            // helper file, and nothing in the brief named that file.
            let mut helper_files: HashMap<String, usize> = HashMap::new();
            // Gate DEFINITION sites: method-shaped gates (check_pr_id,
            // CheckWrite, checkread) are function nodes — one scan maps
            // each gate to the file DEFINING it. That file is the
            // permission catalog/helper class a new gated surface edits
            // (the miss class of two arm-B audits: role.vb,
            // aspnetUsers.vb) — derived from the graph, no name
            // convention needed.
            let mut def_files: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            if !gates.is_empty() {
                let gate_names: HashSet<String> = gates.keys().map(|g| g.to_lowercase()).collect();
                for n in graph
                    .query_nodes(&pid_g, Some("function"), None, None, usize::MAX)
                    .unwrap_or_default()
                {
                    let last = n.name.rsplit('.').next().unwrap_or(&n.name).to_lowercase();
                    if gate_names.contains(&last) {
                        def_files
                            .entry(n.file_path.as_str().replace('\\', "/"))
                            .or_default()
                            .insert(last);
                    }
                    if is_can_helper_name(&n.name) {
                        *helper_files
                            .entry(n.file_path.as_str().replace('\\', "/"))
                            .or_default() += 1;
                    }
                }
            }
            (gates, helper_files, def_files)
        })
        .await
        .unwrap_or_default();
        // Gate types, most frequent first (name asc on ties) — shared by the
        // JSON payload and the markdown section so both state the same cut.
        let gate_rows: Vec<(usize, String)> = {
            let mut rows: Vec<(usize, String)> =
                gates.iter().map(|(g, n)| (*n, g.clone())).collect();
            rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
            rows
        };
        let permission_gates_json = serde_json::json!({
            "cap": GATE_TYPE_CAP,
            "total": gate_rows.len(),
            "shown": gate_rows.len().min(GATE_TYPE_CAP),
            "listed": gate_rows.iter().take(GATE_TYPE_CAP)
                .map(|(n, g)| serde_json::json!({"gate": g, "gated_symbols": n}))
                .collect::<Vec<_>>(),
            "omitted": gate_rows.iter().skip(GATE_TYPE_CAP)
                .map(|(_, g)| g.clone())
                .collect::<Vec<_>>(),
        });

        if req.output_json {
            let (rows, omissions) = change_set_rows(&prov);
            let files: Vec<serde_json::Value> = rows
                .iter()
                .filter(|r| !r.omitted)
                .map(|r| {
                    serde_json::json!({
                        "path": r.path,
                        "layer": r.layer,
                        "tier": r.tier,
                        "signals": r.signals,
                        "why": why.get(&r.path).cloned().unwrap_or_default(),
                        "historical": historical.contains(&r.path),
                    })
                })
                .collect();
            let payload = serde_json::json!({
                "story": req.story.trim(),
                "concepts": concepts,
                "files": files,
                "coverage": cov,
                "omissions": omissions,
                "permission_gates": permission_gates_json,
            });
            return Ok(CallToolResult::success(vec![Content::text(
                serde_json::to_string_pretty(&payload).unwrap_or_default(),
            )]));
        }

        // Scaffold template: when the story ADDS a new API/structural feature,
        // surface the codebase's existing feature COHORT as a template to mirror.
        // Validated to make the agent produce complete, convention-correct new
        // files (controller + service + interface + DTOs + query + registration)
        // instead of an ad-hoc subset. Generic — learns the cohort from the index,
        // no hardcoded layout.
        {
            let sl = req.story.to_lowercase();
            let creation = [
                "add",
                "new ",
                "create",
                "introduce",
                "expose",
                "implement",
                "ability to",
            ]
            .iter()
            .any(|c| sl.contains(c));
            let api = ["api", "endpoint", "rest", "webapi", "web api"]
                .iter()
                .any(|c| sl.contains(c));
            if creation && api {
                // Original-case, web-root-stripped paths (case matters for the
                // interface I-prefix rule inside find_analog_cohort). Dedup: the
                // graph stores some files under two node spellings (Site/-prefixed
                // from index_project, bare from index_git_history) which collapse
                // to the same path here and would otherwise double every cohort row.
                let mut index: Vec<String> = self
                    .state
                    .graph
                    .list_file_node_metadata(&req.project_id)
                    .map(|m| {
                        m.into_iter()
                            .map(|(rp, _)| {
                                let p = rp.as_str().replace('\\', "/");
                                if p.len() >= 5 && p[..5].eq_ignore_ascii_case("site/") {
                                    p[5..].to_string()
                                } else {
                                    p
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                index.sort();
                index.dedup();
                let ranked_canon: HashSet<String> = prov.keys().map(|k| k.to_lowercase()).collect();
                if let Some(cohort) = find_analog_cohort(&index, "controller", &ranked_canon) {
                    let area = cohort
                        .first()
                        .map(|f| {
                            let s: Vec<&str> = f.split('/').collect();
                            if s.len() >= 2 {
                                format!("{}/{}", s[0], s[1]).to_lowercase()
                            } else {
                                String::new()
                            }
                        })
                        .unwrap_or_default();
                    let reg: Vec<&String> = index
                        .iter()
                        .filter(|f| {
                            let fl = f.to_lowercase();
                            fl.starts_with(&area)
                                && ["webapiconfig", "routeconfig", "startup", "global.asax"]
                                    .iter()
                                    .any(|r| fl.contains(r))
                        })
                        .collect();
                    out.push_str(
                        "\n## Likely a NEW feature — scaffold from the codebase's template\n",
                    );
                    out.push_str(
                        "This story adds a new API/structural feature. This codebase builds such a \
                         feature as a FIXED COHORT of files — mirror this complete existing example \
                         for THIS story's entity (same roles, folders, naming convention):\n",
                    );
                    for f in cohort.iter().take(12) {
                        out.push_str(&format!("- `{f}`\n"));
                    }
                    // Spell out the CONCRETE files to create: template rows
                    // alone made agents reverse-engineer the naming and miss
                    // pieces (PR1890: 6 new-domain files, 0 proposed).
                    // Ground the entity in the RANKED file set — the funnel
                    // already knows which files this story is about. Grounding
                    // in the whole index let any long dir containing a story
                    // word win (live: "personalinformation" via the word
                    // "information" from an unrelated customer quote).
                    let ranked_paths: Vec<String> = prov.keys().cloned().collect();
                    let entity_pascal: String = derive_scaffold_entity(&req.story, &ranked_paths)
                        .or_else(|| derive_scaffold_entity(&req.story, &index))
                        .or_else(|| {
                            concepts
                                .iter()
                                .find(|c| c.contains(' '))
                                .or_else(|| concepts.first())
                                .map(|c| {
                                    c.split_whitespace()
                                        .map(|w| {
                                            let mut w = w.to_lowercase();
                                            if let Some(f) = w.get_mut(0..1) {
                                                f.make_ascii_uppercase();
                                            }
                                            w
                                        })
                                        .collect()
                                })
                        })
                        .unwrap_or_default();
                    let proposed = propose_scaffold_paths(&cohort, &entity_pascal);
                    if !proposed.is_empty() {
                        out.push_str(&format!(
                            "Concrete files to CREATE for this story (entity `{entity_pascal}` \
                             derived from the story — adjust the name if the domain calls it \
                             something else):\n"
                        ));
                        for f in proposed.iter().take(12) {
                            out.push_str(&format!("- `{f}`\n"));
                        }
                    }
                    if !reg.is_empty() {
                        out.push_str("Register the new pieces where the codebase wires them up:\n");
                        for f in reg.iter().take(3) {
                            out.push_str(&format!("- `{f}`\n"));
                        }
                    }
                    out.push_str(
                        "Create the ANALOGOUS complete set for the new entity — do NOT omit the \
                         interface, query-params, DTOs, or the registration.\n",
                    );
                    out.push_str(
                        "Beyond the cohort, check the WIRING a live A/B showed even \
                         strong planners miss: (1) permission/role definitions - if \
                         the endpoint is permission-gated, the role catalog file \
                         defining those permissions changes too; (2) the dashboard \
                         page showing the same data - its filter handlers define \
                         your query parameters and often need a shared-service \
                         refactor; (3) DTO VARIANTS - mirror the template's -Out \
                         naming per projection (Item-Out, StatusLog-Out), not one \
                         generic DTO.\n",
                    );
                }
            }
        }

        // Approved-work exemplars: the top merged PRs matching this story are
        // direct evidence for the proposal — their cohorts show the SHAPE of
        // an accepted change (and often contain the files ranking missed).
        // merged_before keeps replays/evals leak-free (see find_merged_work).
        if let Ok(ps) = self.ensure_project_runtime(&req.project_id).await {
            let cutoff_secs = req
                .merged_before
                .as_deref()
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .and_then(crate::handlers::pr_history_tools::ymd_to_epoch_secs);
            // Cutoff rides the indexed timestamp INSIDE the query (strictly
            // before the date): with the old display-time filter, post-cutoff
            // docs ate the top_k slots, so replay exemplars shifted whenever
            // the corpus gained newer PRs. Keep a modest over-fetch for the
            // dedup/malformed-doc cases.
            let fetch_k = if req.merged_before.is_some() { 12 } else { 6 };
            let q = engram_index::HybridQuery {
                project_id: req.project_id.clone(),
                namespace: engram_core::namespaces::NAMESPACE_HISTORY.into(),
                generation: 0,
                text: story_for_concepts(&req.story),
                top_k: fetch_k,
                fts_mode: "loose".into(),
                include_path_prefixes: Some(vec!["pr:".into()]),
                exclude_path_prefixes: None,
                language_filters: None,
                author_filter: None,
                date_after: None,
                date_before: cutoff_secs.map(|s| s.saturating_sub(1)),
                use_mmr: false,
            };
            let engine = ps.search.clone();
            let hits = tokio::task::spawn_blocking(move || engine.lexical_search(&q))
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or_default();
            let cutoff = req
                .merged_before
                .as_deref()
                .map(str::trim)
                .filter(|d| !d.is_empty());
            // Story kind profile from the RANKED candidate set — the same
            // ultra-coarse taxonomy the pr: docs carry on their `kinds:`
            // line. Preferring kind-matching exemplars keeps a UI story
            // from being shaped by API cohorts (and vice versa); lexical
            // rank breaks ties, so with no kind signal the order is
            // unchanged.
            let story_kinds: std::collections::BTreeSet<String> =
                crate::handlers::pr_history_tools::classify_kinds(
                    &prov.keys().cloned().collect::<Vec<_>>(),
                )
                .into_iter()
                .collect();
            let mut seen_doc_ids: HashSet<&str> = HashSet::new();
            let mut docs: Vec<(usize, usize, String)> = Vec::new(); // (kind overlap, lexical rank, content)
            for (rank, h) in hits.iter().enumerate() {
                if !seen_doc_ids.insert(h.doc_id.as_str()) {
                    continue;
                }
                let Ok(Some((_, _, content, _, _))) = ps.search.get_doc_by_doc_id(
                    &req.project_id,
                    engram_core::namespaces::NAMESPACE_HISTORY,
                    0,
                    &h.doc_id,
                ) else {
                    continue;
                };
                if let Some(cut) = cutoff {
                    let leaks = content
                        .lines()
                        .find_map(|l| l.split("merged: ").nth(1))
                        .and_then(|rest| rest.get(..10))
                        .is_none_or(|d| d >= cut);
                    if leaks {
                        continue;
                    }
                }
                let overlap = content
                    .lines()
                    .find_map(|l| l.split("| kinds: ").nth(1))
                    .map(|ks| {
                        ks.split(',')
                            .map(str::trim)
                            .filter(|k| story_kinds.contains(*k))
                            .count()
                    })
                    .unwrap_or(0);
                docs.push((overlap, rank, content));
            }
            // Highest kind overlap first; original lexical rank tiebreaks.
            docs.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            let mut shown = 0usize;
            for (_, _, content) in docs.iter().take(2) {
                if shown == 0 {
                    out.push_str("\n## Approved exemplars — how similar merged work was shaped\n");
                }
                shown += 1;
                // Structure-aware cut: a char-head ends before the file
                // cohort (title/meta → ≤600-char body → cohort), and the
                // cohort IS the payload — see exemplar_view.
                out.push_str(&format!(
                    "{}\n",
                    crate::handlers::pr_history_tools::exemplar_view(content, 20).trim_end()
                ));
            }
            if shown > 0 {
                out.push_str(
                    "next: find_merged_work(story=...) for the complete approved file cohorts.\n",
                );
            }
            // House factoring prior: across ALL fetched similar merged PRs
            // (not just the 2 shown), how did the team LAYER this kind of
            // work — client, server, or both? The factoring fork (same fix,
            // different layer) is the dominant plan-vs-team divergence
            // (arm-B runs 13/15/17: mechanism-correct plans, other layer).
            // Evidence, not a rule: the agent still decides.
            let mut both = 0usize;
            let mut client_only = 0usize;
            let mut server_only = 0usize;
            let mut example = String::new();
            // Per-layer author lists: WHO made each layer choice matters —
            // senior/lead authors' factoring is more authoritative house
            // style than anyone else's (user 2026-07-10). Authority is
            // surfaced generically via the author names + counts; the
            // reader (agent or human) weighs them.
            let mut layer_authors: BTreeMap<&'static str, BTreeMap<String, usize>> =
                BTreeMap::new();
            for (_, _, content) in &docs {
                let meta = content.lines().find(|l| l.contains("| kinds: "));
                let Some(meta) = meta else { continue };
                let Some(kinds_line) = meta.split("| kinds: ").nth(1) else {
                    continue;
                };
                let author = meta
                    .split("| author: ")
                    .nth(1)
                    .and_then(|r| r.split(" |").next())
                    .unwrap_or("?")
                    .trim()
                    .to_string();
                let (c, s) = crate::handlers::pr_history_tools::layer_profile(kinds_line);
                let bucket = match (c, s) {
                    (true, true) => {
                        both += 1;
                        if example.is_empty()
                            && let Some(t) = content.lines().next()
                        {
                            example = t.trim_start_matches('#').trim().to_string();
                        }
                        "client+server"
                    }
                    (true, false) => {
                        client_only += 1;
                        "client-only"
                    }
                    (false, true) => {
                        server_only += 1;
                        "server-only"
                    }
                    (false, false) => continue,
                };
                *layer_authors
                    .entry(bucket)
                    .or_default()
                    .entry(author)
                    .or_default() += 1;
            }
            let total = both + client_only + server_only;
            if total >= 4 {
                out.push_str(&format!(
                    "House factoring prior ({total} similar merged PRs): {both} shipped \
                     CLIENT+SERVER together, {client_only} client-side only, {server_only} \
                     server-side only{}. When your fix could land in either layer, weigh \
                     this team's habit — a mechanism-correct plan in the OTHER layer is \
                     the most common way plans diverge from what actually merged.\n",
                    if example.is_empty() {
                        String::new()
                    } else {
                        format!(" (e.g. {example})")
                    }
                ));
                // WHO made each choice: lead/senior authors' layer choices
                // are the strongest house-style evidence.
                for (bucket, authors) in &layer_authors {
                    let mut rows: Vec<(usize, &String)> =
                        authors.iter().map(|(a, n)| (*n, a)).collect();
                    rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
                    let list: Vec<String> =
                        rows.iter().map(|(n, a)| format!("{a} ({n})")).collect();
                    out.push_str(&format!("  {bucket} by: {}\n", list.join(", ")));
                }
                // Explicit fork callout when the history is genuinely
                // CONTESTED (minority layering >= 1/4 of similar PRs).
                // Authoring experiments (2026-07-10: three arms on a
                // post-knowledge-cutoff bug) showed mechanism-correct
                // implementations landing in the other layer as THE
                // dominant residual divergence from merged work — a
                // product/team decision no evidence derives. What helps
                // is making the choice explicit and reviewable instead
                // of silent.
                let max_bucket = both.max(client_only).max(server_only);
                if (total - max_bucket) * 4 >= total {
                    // Name the MAJORITY layering as the default. The fork
                    // callout's first cut (2026-07-10) only said "pick
                    // one", and a rerun agent confidently chose the 22%
                    // minority layer on a bug whose team fix spanned both
                    // (PR1937: 85.7 -> 50 file-F1). Deviating from a
                    // clear house majority is legitimate but must clear a
                    // higher bar than a coin-flip, so the majority is
                    // stated as the prior and equal splits say so.
                    let (majority, share) = [
                        ("client+server together", both),
                        ("client-side only", client_only),
                        ("server-side only", server_only),
                    ]
                    .into_iter()
                    .max_by_key(|(_, n)| *n)
                    .map(|(label, n)| (label, (n * 100) / total.max(1)))
                    .unwrap_or(("client+server together", 0));
                    let tie = [both, client_only, server_only]
                        .iter()
                        .filter(|&&n| n == max_bucket)
                        .count()
                        > 1;
                    if tie {
                        out.push_str(
                            "  ⚖ FACTORING FORK: this work class has shipped BOTH ways here in \
                             roughly EQUAL measure — the layer choice is a genuine product \
                             decision no evidence settles. Pick ONE deliberately (weigh the \
                             authors above; a client-visible symptom with a server root cause \
                             often warrants BOTH layers), and STATE the choice and its \
                             rationale in your plan and PR description so reviewers judge the \
                             fork instead of discovering it.\n",
                        );
                    } else {
                        out.push_str(&format!(
                            "  ⚖ FACTORING FORK: contested, but the house MAJORITY ({share}%) \
                             ships this work class {majority} — treat that as the DEFAULT. \
                             Deviating to a narrower layer is legitimate only with a specific, \
                             stated reason (e.g. you verified the other layer genuinely has no \
                             defect/role in THIS change); absent that, follow the majority. A \
                             client-visible symptom with a server root cause usually lands in \
                             BOTH layers here. STATE your choice and its rationale so reviewers \
                             judge the fork instead of discovering it.\n"
                        ));
                    }
                }
            }
        }

        // Permission gates in the candidate set: when ranked files carry
        // guard metadata, name the gates - a live A/B showed even strong
        // planners miss permission-catalog changes because nothing in the
        // brief said the surface was gated (PR1890's role.vb lesson).
        {
            if !gates.is_empty() {
                out.push_str(
                    "\n## Permission gates in the candidate set\n\
                     These surfaces are permission/role-gated. Decide explicitly whether \
                     your change needs a NEW permission-catalog entry (and its admin \
                     wiring) or reuses one of these:\n",
                );
                for (n, g) in gate_rows.iter().take(GATE_TYPE_CAP) {
                    out.push_str(&format!("- {g} ({n} gated symbol(s) in the set)\n"));
                }
                if gate_rows.len() > GATE_TYPE_CAP {
                    out.push_str(&format!(
                        "  ... and {} more gate type(s) (cap {GATE_TYPE_CAP}; names in output_json.permission_gates.omitted)\n",
                        gate_rows.len() - GATE_TYPE_CAP
                    ));
                }
                // Definition sites: the file(s) DEFINING these gate checks —
                // a new gated surface usually adds its check/helper THERE.
                let mut df: Vec<(usize, String, Vec<String>)> = gate_def_files
                    .into_iter()
                    .map(|(f, gs)| (gs.len(), f, gs.into_iter().collect()))
                    .collect();
                df.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
                let def_total = df.len();
                for (_, f, gs) in df.into_iter().take(GATE_DEF_FILE_CAP) {
                    out.push_str(&format!(
                        "Gate definitions: `{f}` defines {} — permission-surface changes \
                         usually land there too.\n",
                        gs.join(", ")
                    ));
                }
                if def_total > GATE_DEF_FILE_CAP {
                    out.push_str(&format!(
                        "  ... and {} more gate-definition file(s) (cap {GATE_DEF_FILE_CAP})\n",
                        def_total - GATE_DEF_FILE_CAP
                    ));
                }
                // Only a real convention is worth a line: >=3 Can* helpers
                // concentrated in a file.
                let mut hf: Vec<(usize, String)> = helper_files
                    .into_iter()
                    .filter(|(_, n)| *n >= 3)
                    .map(|(f, n)| (n, f))
                    .collect();
                hf.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
                if !hf.is_empty() {
                    out.push_str(
                        "House convention: permission checks are defined as Can* helpers \
                         in these files — a NEW gated surface usually adds its helper \
                         there (2 live audits missed exactly this):\n",
                    );
                    for (n, f) in hf.into_iter().take(2) {
                        out.push_str(&format!("- `{f}` ({n} Can* helpers)\n"));
                    }
                }
            }
        }

        // Shared components in the candidates' dependency graph: a component
        // IMPORTED BY multiple surfaces is usually where a cross-surface
        // affordance is implemented. Past-PR evidence (arm-B run 9): the
        // team extended the SHARED select control while a locally-scoped
        // plan diverged in 10 files — the extend-vs-local fork needs to be
        // an EXPLICIT decision. Graph-only (O(degree) adjacency); silent
        // when the codebase has no import edges.
        {
            let graph = self.state.graph.clone();
            let pid_s = req.project_id.clone();
            // ALL candidates, not a top-N: adjacency lookups are O(degree)
            // and the shared component often carries a WEAK signal for the
            // story (live: qtyManager.ts was [vector]-only for the RoQ
            // search story yet is exactly the fork that matters).
            let top_files: Vec<String> = prov.keys().cloned().collect();
            // Corroboration per canon path — the same-fan-in tiebreak
            // (an alphabetical tiebreak buried qtyManager.ts behind
            // unrelated fan-in-2 pairs).
            let sig_count: HashMap<String, usize> =
                prov.iter().map(|(k, v)| (k.clone(), v.len())).collect();
            let shared = tokio::task::spawn_blocking(move || {
                let mut out: Vec<(usize, String, Vec<String>)> = Vec::new();
                let mut seen: HashSet<String> = HashSet::new();
                // Both shapes matter: (a) a candidate DEPENDS ON a shared
                // component; (b) the candidate ITSELF is the shared
                // component other surfaces import (the run-9 shape:
                // qtyManager.ts consumed by the map AND fbinstplan
                // surfaces — the fan-in is INCOMING at the candidate).
                let mut check = |node_id: String,
                                 graph: &engram_graph::GraphStore|
                 -> Option<(usize, String, Vec<String>)> {
                    if !seen.insert(node_id.clone()) {
                        return None;
                    }
                    let path = node_id.strip_prefix("file:")?.to_string();
                    // Type declarations are not implementation targets.
                    if path.to_lowercase().ends_with(".d.ts") {
                        return None;
                    }
                    let importers = graph
                        .find_incoming_edges(
                            &pid_s,
                            Some(engram_graph::EdgeKind::Imports),
                            &node_id,
                            25,
                        )
                        .unwrap_or_default();
                    if importers.len() < 2 {
                        return None;
                    }
                    let sample: Vec<String> = importers
                        .iter()
                        .take(3)
                        .filter_map(|(src, _)| src.strip_prefix("file:").map(str::to_string))
                        .collect();
                    Some((importers.len(), path, sample))
                };
                // prov keys are CANON (lowercased, web-root-stripped) but
                // adjacency wants the EXACT node id (original case +
                // prefix). Map via the file-node index: lowercase(+stripped)
                // -> original rel path.
                let mut real: HashMap<String, String> = HashMap::new();
                if let Ok(meta) = graph.list_file_node_metadata(&pid_s) {
                    for (rp, _) in meta {
                        let orig = rp.as_str().replace('\\', "/");
                        let lower = orig.to_lowercase();
                        if let Some(stripped) = lower.strip_prefix("site/") {
                            real.entry(stripped.to_string())
                                .or_insert_with(|| orig.clone());
                        }
                        real.entry(lower).or_insert(orig);
                    }
                }
                for f in &top_files {
                    let Some(orig) = real.get(&f.to_lowercase()) else {
                        continue;
                    };
                    let fid = engram_core::ids::NodeId::file(orig).0;
                    // (b) the candidate itself as the shared component.
                    if let Some(row) = check(fid.clone(), &graph) {
                        out.push(row);
                    }
                    // (a) shared components the candidate depends on.
                    for (dep_id, _w) in graph
                        .neighbors(&pid_s, engram_graph::EdgeKind::Imports, &fid, 20)
                        .unwrap_or_default()
                    {
                        if let Some(row) = check(dep_id, &graph) {
                            out.push(row);
                        }
                    }
                }
                // SPECIFIC-first (fan-in ASCENDING): maximum fan-in =
                // framework hubs (q.ts at 25 = a stopword, the IDF lesson);
                // the story-relevant shared component sits at low fan-in
                // (live: qtyManager.ts at 2 — the map + fbinstplan
                // surfaces). Same fan-in → the component with more story
                // corroboration wins (an alphabetical tiebreak buried
                // qtyManager behind unrelated fan-in-2 pairs).
                out.sort_by(|a, b| {
                    a.0.cmp(&b.0)
                        .then_with(|| {
                            let sa = sig_count.get(&a.1.to_lowercase()).copied().unwrap_or(0);
                            let sb = sig_count.get(&b.1.to_lowercase()).copied().unwrap_or(0);
                            sb.cmp(&sa)
                        })
                        .then_with(|| a.1.cmp(&b.1))
                });
                out.truncate(3);
                out
            })
            .await
            .unwrap_or_default();
            if !shared.is_empty() {
                out.push_str(
                    "\n## Shared components in the candidates' dependency graph\n\
                     These are imported by MULTIPLE surfaces. When the story adds an \
                     affordance to a dialog/control these components implement, decide \
                     EXPLICITLY whether the change belongs IN the shared component \
                     (how past cross-surface work usually shipped) or locally in each \
                     consumer — an unstated fork here diverges the whole file set:\n",
                );
                for (n, comp, sample) in shared {
                    out.push_str(&format!(
                        "- `{comp}` — imported by {n} file(s), e.g. {}\n",
                        sample
                            .iter()
                            .map(|s| format!("`{s}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
        }

        // Review rules for the candidate file families — WRITING-time
        // injection of the distilled review corpus. Replay calibration
        // (2026-07-10, 23 merged PRs / 263 real review findings) showed the
        // post-hoc gates and the reviewers are largely ORTHOGONAL detectors:
        // the classes that cause 3-6 re-push iterations (null-safety,
        // event-API misuse, localization bypass, error handling) are
        // covered by the ingested anti-pattern rules, but only if the
        // agent sees them BEFORE writing the code. So the dossier — the
        // first thing a planning agent reads — carries the relevant rules.
        if let Ok(ps) = self.ensure_project_runtime(&req.project_id).await {
            let q = engram_index::HybridQuery {
                project_id: req.project_id.clone(),
                namespace: engram_core::namespaces::NAMESPACE_ANTIPATTERN.into(),
                generation: 0,
                text: story_for_concepts(&req.story),
                top_k: 12,
                fts_mode: "loose".into(),
                include_path_prefixes: None,
                exclude_path_prefixes: None,
                language_filters: None,
                author_filter: None,
                date_after: None,
                date_before: None,
                use_mmr: false,
            };
            let engine = ps.search.clone();
            let mut hits = tokio::task::spawn_blocking(move || engine.lexical_search(&q))
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or_default();
            // FAMILY-FIRST second pool: story-similarity finds rules about
            // similar FEATURES, but recurring review findings follow CODE
            // SURFACES — api-layer convention rules apply to every API
            // story regardless of topic (authoring experiment 2026-07-10:
            // story-matched-only arming produced zero benefit on PR1874
            // while the finding classes were all api-v2 family rules).
            // Rule docs carry their file-family glob as the synthetic
            // path, so a prefix-filtered query fetches the rules that fire
            // on exactly the directories this change will touch.
            let cand_paths: Vec<String> = prov.keys().map(|k| k.to_lowercase()).collect();
            {
                let mut fam_prefixes: Vec<String> = Vec::new();
                for c in cand_paths.iter().take(12) {
                    let segs: Vec<&str> = c.split('/').collect();
                    if segs.len() >= 2 {
                        let p = segs[..segs.len().min(3) - 1].join("/");
                        if !p.is_empty() && !fam_prefixes.contains(&p) {
                            fam_prefixes.push(p);
                        }
                    }
                }
                fam_prefixes.truncate(6);
                if !fam_prefixes.is_empty() {
                    let fq = engram_index::HybridQuery {
                        project_id: req.project_id.clone(),
                        namespace: engram_core::namespaces::NAMESPACE_ANTIPATTERN.into(),
                        generation: 0,
                        text: story_for_concepts(&req.story),
                        top_k: 12,
                        fts_mode: "loose".into(),
                        include_path_prefixes: Some(fam_prefixes),
                        exclude_path_prefixes: None,
                        language_filters: None,
                        author_filter: None,
                        date_after: None,
                        date_before: None,
                        use_mmr: false,
                    };
                    let engine2 = ps.search.clone();
                    let fam_hits = tokio::task::spawn_blocking(move || engine2.lexical_search(&fq))
                        .await
                        .ok()
                        .and_then(|r| r.ok())
                        .unwrap_or_default();
                    // Family pool first — dedupe by doc_id happens below
                    // via the instruction-dedupe set.
                    let mut merged = fam_hits;
                    merged.extend(hits);
                    hits = merged;
                }
            }
            let glob_prefix = |g: &str| -> String {
                let g = g.to_lowercase().replace('\\', "/");
                g.split("/**")
                    .next()
                    .unwrap_or(&g)
                    .trim_end_matches('/')
                    .to_string()
            };
            let mut rows: Vec<(bool, String, String, Option<String>)> = Vec::new();
            let mut seen_rule: HashSet<String> = HashSet::new();
            for h in hits {
                let Ok(Some((path, _, content, _, _))) = ps.search.get_doc_by_doc_id(
                    &req.project_id,
                    engram_core::namespaces::NAMESPACE_ANTIPATTERN,
                    0,
                    &h.doc_id,
                ) else {
                    continue;
                };
                let instruction = content.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
                if instruction.len() < 12 || !seen_rule.insert(instruction.to_lowercase()) {
                    continue;
                }
                let fix_rate = content
                    .lines()
                    .find(|l| l.starts_with("Fix rate:"))
                    .and_then(|l| l.split('|').next())
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
                // The concrete before→after the team applied (iteration-delta
                // mining): everything after the exemplar marker in the doc.
                let fix_hunk = content
                    .split_once("House fix (applied in a merged PR):")
                    .map(|(_, h)| h.trim().to_string())
                    .filter(|h| !h.is_empty());
                let prefix = glob_prefix(path.as_str());
                let file_match = !prefix.is_empty()
                    && cand_paths.iter().any(|c| {
                        c.starts_with(&prefix)
                            || c.strip_prefix("site/")
                                .is_some_and(|s| s.starts_with(&prefix))
                            || prefix
                                .strip_prefix("site/")
                                .is_some_and(|p| c.starts_with(p))
                    });
                rows.push((
                    file_match,
                    format!("`{}`", path.as_str()),
                    format!(
                        "{instruction}{}",
                        if fix_rate.is_empty() {
                            String::new()
                        } else {
                            format!(" ({})", fix_rate.to_lowercase())
                        }
                    ),
                    fix_hunk,
                ));
            }
            // Candidate-matching rules first; among those, rules that carry a
            // concrete fix exemplar rank ahead (they're the most actionable).
            rows.sort_by(|a, b| {
                b.0.cmp(&a.0)
                    .then_with(|| b.3.is_some().cmp(&a.3.is_some()))
            });
            rows.truncate(8);
            if !rows.is_empty() {
                out.push_str(
                    "\n## Review rules for this change (distilled from this repo's past code reviews)\n\
                     Reviewers flagged these issue classes repeatedly — in the file families \
                     marked ▲ they fired on the very files this change ranks. Write the code \
                     so they never fire; each one caught late costs a review round-trip. \
                     Where a ‹house fix› is shown, that is the exact change the team applied \
                     last time — mirror its approach:\n",
                );
                // Show the concrete fix hunk for the top few file-matched
                // rules that carry one; keep the rest as one-liners so the
                // section stays scannable.
                let mut hunks_shown = 0usize;
                for (matched, family, rule, fix_hunk) in rows {
                    out.push_str(&format!(
                        "- {}{family}: {rule}\n",
                        if matched { "▲ " } else { "" }
                    ));
                    if hunks_shown < 3
                        && matched
                        && let Some(hunk) = fix_hunk
                    {
                        let trimmed: String = hunk.lines().take(14).collect::<Vec<_>>().join("\n");
                        out.push_str("  ‹house fix›\n```diff\n");
                        out.push_str(&trimmed);
                        out.push_str("\n```\n");
                        hunks_shown += 1;
                    }
                }
            }
        }

        // The ranked file set is the single most freshness-sensitive output
        // in the funnel: a stale index proposes the wrong files. This was
        // the only primary funnel tool with no staleness signal.
        let gen_ = self
            .get_active_generation(&req.project_id)
            .await
            .unwrap_or(1);
        out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    /// TODO-29: the loop-closing check. Given the files an agent edited,
    /// report what history and the graph say should ALSO have changed:
    /// strong co-change partners left untouched, and state keys whose other
    /// readers/writers live outside the edit set.
    pub async fn handle_detect_incomplete_changes(
        &self,
        req: crate::models::DetectIncompleteChangesRequest,
    ) -> Result<CallToolResult, McpError> {
        self.detect_incomplete_changes_with(req, None).await
    }

    /// `detect_incomplete_changes` with an optional shared node snapshot so a
    /// composite call (get_change_set runs this 2-3 times) scans the project
    /// ONCE instead of once per pass.
    pub(crate) async fn detect_incomplete_changes_with(
        &self,
        req: crate::models::DetectIncompleteChangesRequest,
        snapshot: Option<NodeSnapshot>,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        if req.edited_files.is_empty() {
            return Err(McpError::invalid_params(
                "edited_files must not be empty".to_string(),
                None,
            ));
        }
        let _ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;

        let rec = self.ensure_project_record(&req.project_id).await?;
        let project_dir = PathBuf::from(&rec.directory);
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let edited: Vec<String> = req
            .edited_files
            .iter()
            // Normalise to forward slashes AND strip any leading "/" — callers
            // (and PR ground-truth) often pass "/Site/...", but file node ids
            // are "Site/..."; an unstripped leading slash silently resolves to
            // nothing and the audit returns a false "all clear".
            .map(|f| f.replace('\\', "/").trim_start_matches('/').to_string())
            .collect();
        let max_partners = req.max_partners.clamp(1, 20);

        let (partner_findings, state_findings, unwired_findings, unresolved_inputs) =
            tokio::task::spawn_blocking(move || {
                let edited_set: HashSet<String> = edited.iter().map(|f| f.to_lowercase()).collect();
                // TemporalCoupling nodes are keyed by REAL git case, but callers
                // (e.g. get_change_set's path extractor) may pass lowercased paths.
                // An exact-match neighbour lookup then silently misses every
                // PascalCase file — i.e. most .NET class files (SystemSettingStore.vb
                // etc.), the very hub files whose co-change family the caller needs.
                // Resolve each edited path to its real-case node id once. Generic:
                // any case-insensitive caller against a case-sensitive graph.
                let real_case: HashMap<String, String> = graph
                    .list_file_node_metadata(&pid)
                    .map(|m| {
                        m.into_iter()
                            .map(|(rp, _)| {
                                let r = rp.as_str().replace('\\', "/");
                                (r.to_lowercase(), r)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                // A path counts as "covered" if any edited path suffix-matches
                // it, component-aligned (handles Site/-prefix spelling
                // variants from pre-restructure history). Shared logic with
                // pre_commit_review's temporal gate — see
                // `pre_commit_review_service::path_suffix_match`.
                let covered =
                    |path: &str| -> bool { edited_set.iter().any(|e| path_suffix_match(e, path)) };
                // Current-tree spellings — co-change partners from history
                // may predate a repo restructure and must be re-anchored to
                // the file that exists today (or dropped entirely).
                let current_files: Vec<String> = real_case.values().cloned().collect();

                // ── Co-change partners not in the edit set ──────────────────
                // Collect raw candidates (edited_file, current-tree partner,
                // weight, raw graph spelling); weak couplings are noise, so
                // demand real history.
                let mut raw: Vec<(String, String, u32, String)> = Vec::new();
                let mut unresolved_inputs: Vec<String> = Vec::new();
                for f in &edited {
                    let resolved = match real_case.get(&f.to_lowercase()) {
                        Some(r) => r.as_str(),
                        None => {
                            // A completeness tool that silently skips an input
                            // file can print "looks complete" while having seen
                            // NOTHING — flag it instead.
                            unresolved_inputs.push(f.clone());
                            f.as_str()
                        }
                    };
                    let fid = format!("file:{resolved}");
                    let Ok(neigh) = graph.neighbors(&pid, EdgeKind::TemporalCoupling, &fid, 500)
                    else {
                        continue;
                    };
                    for (nid, weight) in neigh {
                        let Some(partner) = nid.strip_prefix("file:") else {
                            continue;
                        };
                        if weight < 5 || covered(partner) {
                            continue;
                        }
                        // Never emit a historical spelling: re-anchor the
                        // partner to its current-tree file, or drop it when
                        // the file no longer exists under any spelling. This
                        // is also what surfaces genuine gaps that a raw
                        // string comparison used to suppress (the partner
                        // wasn't textually equal to anything covered, but
                        // wasn't textually equal to anything real either).
                        let Some(current) =
                            resolve_partner_to_current(partner, &current_files, &project_dir)
                        else {
                            continue;
                        };
                        if covered(&current) {
                            continue;
                        }
                        raw.push((f.clone(), current, weight, partner.to_string()));
                    }
                }
                // Keep the single strongest coupling per (current-tree)
                // partner — multiple historical spellings collapse here.
                raw.sort_by(|a, b| b.2.cmp(&a.2));
                let mut best_per_partner: Vec<(String, String, u32, String)> = Vec::new();
                {
                    let mut seen: HashSet<String> = HashSet::new();
                    for r in raw {
                        if seen.insert(r.1.to_lowercase()) {
                            best_per_partner.push(r);
                        }
                    }
                }
                // Hub down-weighting: a partner that co-changes with a HUGE number of
                // DISTINCT files (a global resx bundle, a shared script bundle) is
                // touched by almost every change, so it carries no specific "you
                // missed this companion" signal — surfacing it only adds noise and
                // steers the agent toward the wrong family (e.g. label.resx when the
                // change actually needs text.resx). Drop partners whose co-change
                // DEGREE marks them ubiquitous. Generic — degree-based, the same IDF
                // insight as the cross-section map; no per-repo names. The default
                // is a high floor so it NO-OPS on sparse/young repos (no file reaches
                // it) and only trims genuinely ubiquitous hubs on dense histories
                // like this one (label.resx ~1063, text.resx ~900 get dropped;
                // moderately-specific companions ~300-700 survive). Env-overridable.
                let hub_degree: usize = std::env::var("ENGRAM_HUB_DEGREE")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(800);
                // Co-change degree per candidate partner (distinct
                // neighbours). Must use the RAW graph spelling — the
                // TemporalCoupling adjacency is keyed by whatever spelling
                // existed at commit time, not the re-anchored current path.
                let degree_of = |raw_partner: &str| -> usize {
                    let pfid = format!("file:{raw_partner}");
                    graph
                        .neighbors(&pid, EdgeKind::TemporalCoupling, &pfid, 2000)
                        .map(|v| v.len())
                        .unwrap_or(0)
                };
                let mut partner_findings: Vec<(String, String, u32, usize)> = best_per_partner
                    .into_iter()
                    .map(|(e, current, w, raw_partner)| {
                        let d = degree_of(&raw_partner);
                        (e, current, w, d)
                    })
                    .filter(|(_, _, _, d)| *d < hub_degree)
                    .collect();
                partner_findings.truncate(max_partners);

                // ── State keys shared with untouched files ──────────────────
                // Symbols in edited files -> state targets -> other touchers.
                let mut state_findings: Vec<(String, String)> = Vec::new();
                let mut seen_keys: HashSet<String> = HashSet::new();
                let nodes: NodeSnapshot = match snapshot {
                    Some(shared) => shared,
                    None => std::sync::Arc::new(
                        graph
                            .query_nodes(&pid, None, None, None, crate::handlers::NODE_SCAN_LIMIT)
                            .unwrap_or_default(),
                    ),
                };
                let edited_symbol_ids: Vec<String> = nodes
                    .iter()
                    .filter(|n| covered(n.file_path.as_str()))
                    .map(|n| n.node_id.clone())
                    .collect();
                let node_file: std::collections::HashMap<&str, &str> = nodes
                    .iter()
                    .map(|n| (n.node_id.as_str(), n.file_path.as_str()))
                    .collect();
                for sid in edited_symbol_ids.iter().take(500) {
                    for kind in [EdgeKind::ReadsState, EdgeKind::WritesState] {
                        let Ok(neigh) = graph.neighbors(&pid, kind, sid, 20) else {
                            continue;
                        };
                        for (state_id, _) in neigh {
                            if !state_id.starts_with("state:")
                                || !seen_keys.insert(state_id.clone())
                            {
                                continue;
                            }
                            // Other touchers of this key outside the edit set.
                            let Ok(touchers) =
                                graph.find_incoming_edges_with_kind(&pid, None, &state_id, 100)
                            else {
                                continue;
                            };
                            let outside: Vec<&str> = touchers
                                .iter()
                                .filter_map(|(src, _, _)| node_file.get(src.as_str()).copied())
                                .filter(|f| !covered(f))
                                .take(3)
                                .collect();
                            if !outside.is_empty() {
                                state_findings.push((
                                    state_id
                                        .strip_prefix("state:")
                                        .unwrap_or(&state_id)
                                        .to_string(),
                                    outside.join(", "),
                                ));
                            }
                        }
                    }
                }
                state_findings.truncate(10);

                // ── Implemented but never wired (0 callers) ──────────────────
                // Real failure class: a branch adds public methods whose doc
                // comments CLAIM callers ("Used by the X gate"), but the graph
                // shows ZERO incoming call edges — the ruled behavior was never
                // wired up. Generic signal: a new/changed method nobody calls
                // is dead scaffolding or unfinished wiring; either way a
                // reviewer must see it. Reuses find_dead_methods' exclusions —
                // framework-invoked kinds (Lifecycle/ControlEvent/WebMethod)
                // and Handles-clause methods never have static callers, so
                // flagging them would be pure noise.
                let mut unwired_findings: Vec<(String, String, u32)> = Vec::new();
                let changed_fns = nodes
                    .iter()
                    .filter(|n| n.node_type == "function" && covered(n.file_path.as_str()));
                for node in changed_fns.take(500) {
                    let effects = node_meta_csv(node, "effects");
                    let kind =
                        full_mig::classify_method_kind_pub(&node.name, &effects, &node.metadata)
                            .to_string();
                    let has_handles = !node_meta_csv(node, "handles_clause").is_empty();
                    let caller_count =
                        crate::handlers::incoming_caller_edges(&graph, &pid, &node.node_id, 1)
                            .len();
                    if unwired_should_flag(&kind, has_handles, caller_count) {
                        unwired_findings.push((
                            node_display_name(node),
                            node.file_path.as_str().replace('\\', "/"),
                            node.start_line,
                        ));
                        if unwired_findings.len() >= 10 {
                            break;
                        }
                    }
                }

                (
                    partner_findings,
                    state_findings,
                    unwired_findings,
                    unresolved_inputs,
                )
            })
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let mut out = String::from("# Edit completeness check\n");
        if !unresolved_inputs.is_empty() {
            out.push_str(&format!(
                "\n⚠ {} of your edited files were NOT found in the index ({}) — typo, \
                 moved, or not yet indexed. Findings below may be incomplete; run \
                 update_project if the files are new.\n",
                unresolved_inputs.len(),
                unresolved_inputs
                    .iter()
                    .take(5)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if partner_findings.is_empty() && state_findings.is_empty() && unwired_findings.is_empty() {
            out.push_str(
                "\nNo strong co-change or shared-state links point outside your edit set. \
                 NOTE: co-change needs ingested git history (index_git_history) — if it was \
                 never ingested this means 'no data to check', not 'confirmed complete'. \
                 Confirm sibling-completeness (H-class rules: other call sites, both master \
                 pages) with grep_project / a working-tree grep as well.\n",
            );
        } else {
            if !partner_findings.is_empty() {
                out.push_str("\n## Co-change partners you did NOT touch\n");
                out.push_str(
                    "History says these files change together with yours — verify each:\n",
                );
                for (edited, partner, weight, _degree) in &partner_findings {
                    out.push_str(&format!(
                        "- `{partner}` ({weight} co-changes with `{edited}`)\n"
                    ));
                }
            }
            if !state_findings.is_empty() {
                out.push_str("\n## Shared state with untouched files\n");
                for (key, files) in &state_findings {
                    out.push_str(&format!(
                        "- state key `{key}` is also read/written in: {files}\n"
                    ));
                }
            }
            if !unwired_findings.is_empty() {
                out.push_str("\n## Implemented but never wired (0 callers)\n");
                out.push_str(
                    "New/changed methods nobody calls are dead scaffolding or unfinished \
                     wiring — verify the call sites this method was built for actually exist:\n",
                );
                for (name, file, line) in &unwired_findings {
                    out.push_str(&format!("- `{name}` ({file}:{line})\n"));
                }
            }
        }
        // Deliberately does NOT chain to find_similar_changes: this tool has
        // already answered the companion-artifact question from precomputed
        // temporal-coupling edges, while find_similar_changes re-walks git
        // history. Agents followed the hint straight into that walk.
        // House conventions: surface the CodeRabbit-derived repo rules whose
        // pattern matches any edited file, ONCE for the whole changeset. These
        // are the team's tacit conventions (promoted by ingest_code_review_
        // history) that a new dev does not know yet — the class this tool exists
        // to shift left. This is the only place the review flow sees them.
        {
            use crate::utils::files::pattern_match;
            let reg = self.state.registry.clone();
            let pid = req.project_id.clone();
            let rules = tokio::task::spawn_blocking(move || reg.list_repo_rules(&pid))
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or_default();
            let mut applicable: Vec<String> = Vec::new();
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            for f in &req.edited_files {
                for r in &rules {
                    if pattern_match(f, &r.file_pattern) && seen.insert(r.rule_id.clone()) {
                        applicable.push(r.rule_text.clone());
                    }
                }
            }
            if !applicable.is_empty() {
                out.push_str(&format!(
                    "\n## House conventions for these files ({})\nLearned from this repo's \
                     CodeRabbit history — check each change against them:\n",
                    applicable.len()
                ));
                for t in applicable.iter().take(40) {
                    out.push_str(&format!("- {t}\n"));
                }
            }
        }
        out.push_str("\nnext: pre_commit_review before committing.\n");
        out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }
}

/// Decide whether a changed function node should be flagged as "implemented
/// but never wired". Mirrors the find_dead_methods exclusion classes:
/// framework-invoked kinds (Lifecycle, ControlEvent, WebMethod) and methods
/// bound via a VB `Handles` clause are invoked by the runtime, never by code,
/// so a zero static-caller count is expected for them — flag only the rest.
pub(crate) fn unwired_should_flag(
    method_kind: &str,
    has_handles_clause: bool,
    incoming_caller_count: usize,
) -> bool {
    !matches!(method_kind, "Lifecycle" | "ControlEvent" | "WebMethod")
        && !has_handles_clause
        && incoming_caller_count == 0
}

/// Comma-separated metadata field from a graph node ("effects",
/// "handles_clause", …) as a Vec of trimmed non-empty entries.
fn node_meta_csv(node: &engram_graph::Node, key: &str) -> Vec<String> {
    node.metadata
        .as_ref()
        .and_then(|m| m.get(key))
        .and_then(|v| v.as_str())
        .map(|s| {
            s.split(',')
                .map(|e| e.trim().to_string())
                .filter(|e| !e.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// "Class.Method" display name for a function node — the namespace usually
/// holds the class; skip it when empty, the parser default, or a SEARCH
/// namespace constant ("memory", …) that ingest stores there while the
/// name is already fully qualified (rendering it produced
/// `memory._ata.ChangeRequestMarker.Create`).
fn node_display_name(node: &engram_graph::Node) -> String {
    let ns = node.namespace.trim();
    if ns.is_empty() || ns == "default" || engram_core::namespaces::KNOWN_NAMESPACES.contains(&ns) {
        node.name.clone()
    } else {
        format!("{}.{}", ns, node.name)
    }
}

const EDIT_SESSION_META_KEY: &str = "edit_session_v1";

impl Engram {
    /// TODO-29: open the edit-session bookend. Persists intent and returns
    /// the expectation brief (partners + state couplings of the planned
    /// files) BEFORE any line changes — so the agent knows the blast
    /// surface going in, not after.
    pub async fn handle_begin_edit_session(
        &self,
        req: crate::models::BeginEditSessionRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        if req.planned_files.is_empty() {
            return Err(McpError::invalid_params(
                "planned_files must not be empty".to_string(),
                None,
            ));
        }
        let session = serde_json::json!({
            "planned_files": req.planned_files,
            "story": req.story,
            "started_ms": crate::utils::now_ms(),
        });
        let reg = self.state.registry.clone();
        let pid = req.project_id.clone();
        let payload = session.to_string();
        tokio::task::spawn_blocking(move || reg.set_meta(&pid, EDIT_SESSION_META_KEY, &payload))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // The expectation brief is the same engine, run on the PLANNED set:
        // anything it reports now is coupling the agent should plan for.
        let brief = self
            .handle_detect_incomplete_changes(crate::models::DetectIncompleteChangesRequest {
                project_id: req.project_id.clone(),
                edited_files: req.planned_files.clone(),
                max_partners: 5,
            })
            .await?;
        let brief_text = brief
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");

        Ok(CallToolResult::success(vec![Content::text(format!(
            "# Edit session OPEN\nplanned files: {}\n\n## Expectation brief \
             (couplings of your planned set — plan for these now)\n{}\n\
             next: edit; then complete_edit_session(edited_files=[...]) before committing.\n",
            req.planned_files.join(", "),
            brief_text
        ))]))
    }

    /// TODO-29: close the bookend — completeness check against the actual
    /// edit set plus drift vs the original plan, then clear the session.
    pub async fn handle_complete_edit_session(
        &self,
        req: crate::models::CompleteEditSessionRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let reg = self.state.registry.clone();
        let pid = req.project_id.clone();
        let stored = tokio::task::spawn_blocking(move || reg.get_meta(&pid, EDIT_SESSION_META_KEY))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let Some(stored) = stored.filter(|s| !s.trim().is_empty()) else {
            return Err(McpError::invalid_params(
                "no open edit session — call begin_edit_session first (or use \
                 detect_incomplete_changes directly for a stateless check)"
                    .to_string(),
                None,
            ));
        };
        let session: serde_json::Value = serde_json::from_str(&stored)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let planned: Vec<String> = session["planned_files"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let edited = if req.edited_files.is_empty() {
            planned.clone()
        } else {
            req.edited_files.clone()
        };

        // Plan drift: planned-but-not-edited is the silent scope shrink that
        // reviews catch late.
        let edited_lower: HashSet<String> = edited.iter().map(|f| f.to_lowercase()).collect();
        let unedited_plan: Vec<&String> = planned
            .iter()
            .filter(|f| !edited_lower.contains(&f.to_lowercase()))
            .collect();

        let check = self
            .handle_detect_incomplete_changes(crate::models::DetectIncompleteChangesRequest {
                project_id: req.project_id.clone(),
                edited_files: edited.clone(),
                max_partners: 5,
            })
            .await?;
        let check_text = check
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");

        // Session consumed either way — completing twice is a usage error.
        let reg2 = self.state.registry.clone();
        let pid2 = req.project_id.clone();
        tokio::task::spawn_blocking(move || reg2.set_meta(&pid2, EDIT_SESSION_META_KEY, ""))
            .await
            .ok();

        let mut out = String::from("# Edit session COMPLETE\n");
        if !unedited_plan.is_empty() {
            out.push_str("\n## Planned but NOT edited (scope drift — confirm intentional)\n");
            for f in unedited_plan {
                out.push_str(&format!("- {f}\n"));
            }
        }
        // Dossier-obligation reconciliation: the dossier the agent
        // implemented from IS the contract — every file it referenced in
        // its structured sections is an obligation. Naming the unmet ones
        // here closes the loop that one-shot implementations were missing
        // (agents silently skipped dossier items and nothing ever checked).
        if let Some(ref dossier) = req.dossier {
            let obligations = extract_dossier_obligations(dossier);
            if !obligations.is_empty() {
                let met: Vec<&(String, String)> = obligations
                    .iter()
                    .filter(|(_, f)| {
                        edited.iter().any(|e| {
                            crate::services::pre_commit_review_service::path_suffix_match(e, f)
                        })
                    })
                    .collect();
                let unmet: Vec<&(String, String)> = obligations
                    .iter()
                    .filter(|(_, f)| {
                        !edited.iter().any(|e| {
                            crate::services::pre_commit_review_service::path_suffix_match(e, f)
                        })
                    })
                    .collect();
                out.push_str(&format!(
                    "\n## Dossier reconciliation ({} of {} referenced files touched)\n",
                    met.len(),
                    obligations.len()
                ));
                if !unmet.is_empty() {
                    out.push_str(
                        "Referenced by the dossier but NOT in your edit set — address \
                         each or state why it doesn't apply:\n",
                    );
                    for (section, f) in unmet.iter().take(30) {
                        out.push_str(&format!("- `{f}` [{section}]\n"));
                    }
                }
            }
        }
        out.push_str("\n");
        out.push_str(&check_text);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }
}

/// Work-item id from a story: "#847", "Bug 847", "US 1234", "AB#847".
/// Requires an id-ish keyword or # so bare numbers in prose don't match.
pub(crate) fn extract_work_item_id(story: &str) -> Option<u64> {
    use std::sync::LazyLock;
    static RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
            r"(?i)(?:\b(?:bug|us|user story|story|item|task|ab)\s*#?\s*|#)(\d{2,7})\b",
        )
        .expect("valid regex")
    });
    RE.captures(story)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse().ok())
}

/// Concept extraction must not see the scaffolding labels this handler
/// injects around fetched work-item text: live verify (pilot corpus Bug #847)
/// showed the header tokens — "work", "item", "full" — outranking the
/// story's actual domain concepts. The rendered brief keeps the labels;
/// extraction gets this stripped view.
pub(crate) fn story_for_concepts(story: &str) -> String {
    let s = story
        .replace("## Work item (full text)", "")
        .replace("Acceptance criteria:", "");
    // URLs are not domain concepts: a pasted support-ticket link made its
    // hostname/path tokens 2 of the 5 extracted
    // concepts on a live fetch. Drop whole URL tokens.
    s.split_whitespace()
        .filter(|w| !w.starts_with("http://") && !w.starts_with("https://"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Azure DevOps (org, project) from a git remote URL. Handles the three
/// remote shapes ADO issues: modern HTTPS (`https://[user@]dev.azure.com/
/// {org}/{project}/_git/{repo}`), SSH (`git@ssh.dev.azure.com:v3/{org}/
/// {project}/{repo}`), and legacy (`https://{org}.visualstudio.com/
/// [DefaultCollection/]{project}/_git/{repo}`). Non-ADO remotes → None.
pub(crate) fn ado_coords_from_remote_url(url: &str) -> Option<(String, String)> {
    let url = url.trim().trim_end_matches('/').trim_end_matches(".git");
    // SSH first — its host also contains "dev.azure.com" but with ':' not '/'.
    if let Some(rest) = url.split("ssh.dev.azure.com:v3/").nth(1) {
        let mut seg = rest.split('/');
        let (org, project, repo) = (seg.next()?, seg.next()?, seg.next()?);
        if !org.is_empty() && !project.is_empty() && !repo.is_empty() {
            return Some((org.to_string(), project.to_string()));
        }
        return None;
    }
    if let Some(rest) = url.split("dev.azure.com/").nth(1) {
        let mut seg = rest.split('/');
        let (org, project) = (seg.next()?, seg.next()?);
        if !org.is_empty() && !project.is_empty() && seg.next() == Some("_git") {
            return Some((org.to_string(), project.to_string()));
        }
        return None;
    }
    if let Some((host, rest)) = url.strip_prefix("https://").and_then(|u| u.split_once('/'))
        && let Some(org) = host.strip_suffix(".visualstudio.com")
    {
        let mut seg = rest.split('/');
        let mut project = seg.next()?;
        if project == "DefaultCollection" {
            project = seg.next()?;
        }
        if !org.is_empty() && !project.is_empty() && seg.next() == Some("_git") {
            return Some((org.to_string(), project.to_string()));
        }
    }
    None
}

/// `remote.origin.url` read straight from `.git/config` (no subprocess).
/// Follows a `.git` POINTER FILE (worktrees/submodules: "gitdir: <path>"),
/// where the shared config sits two levels above the per-worktree gitdir.
pub(crate) fn git_remote_origin_url(root: &std::path::Path) -> Option<String> {
    let dot_git = root.join(".git");
    let config_path = if dot_git.is_file() {
        let content = std::fs::read_to_string(&dot_git).ok()?;
        let gitdir = content.strip_prefix("gitdir:")?.trim();
        let gitdir = if std::path::Path::new(gitdir).is_absolute() {
            std::path::PathBuf::from(gitdir)
        } else {
            root.join(gitdir)
        };
        let local = gitdir.join("config");
        if local.exists() {
            local
        } else {
            gitdir.parent()?.parent()?.join("config")
        }
    } else {
        dot_git.join("config")
    };
    parse_origin_url(&std::fs::read_to_string(config_path).ok()?)
}

/// Minimal .git/config scan: the first `url =` inside `[remote "origin"]`.
pub(crate) fn parse_origin_url(cfg: &str) -> Option<String> {
    let mut in_origin = false;
    for line in cfg.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_origin = t.eq_ignore_ascii_case(r#"[remote "origin"]"#);
        } else if in_origin
            && let Some(v) = t.strip_prefix("url")
            && let Some(v) = v.trim_start().strip_prefix('=')
        {
            return Some(v.trim().to_string());
        }
    }
    None
}

/// PAT source for the work-item auto-fetch: per-call wins, else the
/// server's own `ADO_PAT` env var (the daemon's deployment environment —
/// the same convention the ADO eval/corpora scripts use). Nothing is
/// ever persisted. The env fallback is what makes the auto-fetch fire in
/// REAL agent sessions: agents don't hold credentials, the host does.
fn resolve_ado_pat(per_call: Option<String>) -> Option<String> {
    pick_pat(per_call, std::env::var("ADO_PAT").ok())
}

/// Pure precedence: first non-blank of (per-call, env fallback), trimmed.
fn pick_pat(per_call: Option<String>, env_fallback: Option<String>) -> Option<String> {
    per_call
        .into_iter()
        .chain(env_fallback)
        .map(|p| p.trim().to_string())
        .find(|p| !p.is_empty())
}

/// Fetch an ADO work item's full text (title + description/repro +
/// acceptance criteria, HTML stripped). None on ANY failure — callers
/// degrade to the story alone.
async fn fetch_ado_work_item(org: &str, project: &str, id: u64, pat: &str) -> Option<String> {
    // $expand=relations: real defects come as LINKED CLUSTERS — the eval's
    // four-arm study measured a single missing sibling bug at -45.7 F1
    // (PR1937: two linked bugs, one symptom each). Input parity means the
    // whole cluster, exactly what the dev sees on the item.
    let url = format!(
        "https://dev.azure.com/{org}/{project}/_apis/wit/workitems/{id}?api-version=7.0&$expand=relations"
    );
    let auth = base64_encode(format!(":{pat}").as_bytes());
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .ok()?;
    let resp = client
        .get(&url)
        .header("Authorization", format!("Basic {auth}"))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        tracing::info!(id, status = %resp.status(), "work-item auto-fetch failed");
        return None;
    }
    let v: serde_json::Value = resp.json().await.ok()?;
    let f = v.get("fields")?;
    let get = |k: &str| f.get(k).and_then(|x| x.as_str()).unwrap_or("");
    let title = get("System.Title");
    let wtype = get("System.WorkItemType");
    let desc = {
        let d = get("System.Description");
        if d.is_empty() {
            get("Microsoft.VSTS.TCM.ReproSteps")
        } else {
            d
        }
    };
    let accept = get("Microsoft.VSTS.Common.AcceptanceCriteria");
    let mut out = format!("[{wtype} #{id}] {title}\n\n{}", strip_html(desc));
    if !accept.is_empty() {
        // AC provenance (fail-soft): one-liner stories sometimes get
        // acceptance criteria back-filled by the implementer (often
        // AI-assisted) after the team wrote the story. Those are hints
        // to verify, not team-committed spec — and an agent that treats
        // them as spec faithfully implements criteria the team never
        // agreed to. Label who wrote the AC and flag back-fills.
        let label = match fetch_ac_provenance(&client, org, project, id, &auth).await {
            Some((ac_author, ac_date, creator)) => {
                if !creator.is_empty() && ac_author != creator {
                    format!(
                        "Acceptance criteria (written by {ac_author} on {ac_date}; \
                         the story was created by {creator} — these criteria were \
                         back-filled later; verify them against the description and \
                         existing merged work rather than treating them as \
                         team-committed spec)"
                    )
                } else {
                    format!("Acceptance criteria (written by {ac_author} on {ac_date})")
                }
            }
            None => "Acceptance criteria".to_string(),
        };
        out.push_str(&format!("\n\n{label}:\n{}", strip_html(accept)));
    }
    // Linked work items (bounded, fail-soft): titles + trimmed descriptions.
    for lid in extract_relation_ids(&v, id).into_iter().take(3) {
        let lurl = format!(
            "https://dev.azure.com/{org}/{project}/_apis/wit/workitems/{lid}?api-version=7.0"
        );
        let Ok(lresp) = client
            .get(&lurl)
            .header("Authorization", format!("Basic {auth}"))
            .send()
            .await
        else {
            continue;
        };
        if !lresp.status().is_success() {
            continue;
        }
        let Ok(lv) = lresp.json::<serde_json::Value>().await else {
            continue;
        };
        let Some(lf) = lv.get("fields") else { continue };
        let lget = |k: &str| lf.get(k).and_then(|x| x.as_str()).unwrap_or("");
        let ltitle = lget("System.Title");
        if ltitle.is_empty() {
            continue;
        }
        let ldesc = {
            let d = lget("System.Description");
            if d.is_empty() {
                lget("Microsoft.VSTS.TCM.ReproSteps")
            } else {
                d
            }
        };
        let ltype = lget("System.WorkItemType");
        let body: String = strip_html(ldesc).chars().take(1200).collect();
        out.push_str(&format!("\n\n[linked {ltype} #{lid}] {ltitle}\n{body}"));
    }
    Some(out)
}

/// Who first wrote the acceptance criteria, and who created the item.
///
/// Reads the work-item revision history (`/updates`) and returns
/// `(ac_author, ac_date_yyyy_mm_dd, item_creator)` for the first
/// revision that populated AcceptanceCriteria. None on any failure or
/// if the field never appears in history — callers fall back to an
/// unannotated label.
async fn fetch_ac_provenance(
    client: &reqwest::Client,
    org: &str,
    project: &str,
    id: u64,
    auth: &str,
) -> Option<(String, String, String)> {
    let url = format!(
        "https://dev.azure.com/{org}/{project}/_apis/wit/workitems/{id}/updates?api-version=7.0&$top=200"
    );
    let resp = client
        .get(&url)
        .header("Authorization", format!("Basic {auth}"))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: serde_json::Value = resp.json().await.ok()?;
    let updates = v.get("value")?.as_array()?;
    let mut creator = String::new();
    for u in updates {
        let who = u
            .get("revisedBy")
            .and_then(|b| b.get("displayName"))
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .to_string();
        if u.get("rev").and_then(|r| r.as_u64()) == Some(1) {
            creator = who.clone();
        }
        let Some(fields) = u.get("fields") else {
            continue;
        };
        let Some(ac) = fields.get("Microsoft.VSTS.Common.AcceptanceCriteria") else {
            continue;
        };
        let new_val = ac.get("newValue").and_then(|x| x.as_str()).unwrap_or("");
        let old_val = ac.get("oldValue").and_then(|x| x.as_str()).unwrap_or("");
        if new_val.trim().is_empty() || !old_val.trim().is_empty() {
            continue; // want the revision that FIRST populated the field
        }
        // Prefer the field-level ChangedDate; revisedDate is 9999-01-01
        // on some in-flight revisions.
        let date = fields
            .get("System.ChangedDate")
            .and_then(|c| c.get("newValue"))
            .and_then(|d| d.as_str())
            .or_else(|| u.get("revisedDate").and_then(|d| d.as_str()))
            .unwrap_or("");
        let date = date.get(..10).unwrap_or(date).to_string();
        if who.is_empty() {
            return None;
        }
        return Some((who, date, creator));
    }
    None
}

/// Work-item ids from an item's `relations` array (System.LinkTypes.* only —
/// attachments/hyperlinks/commits carry other rel values). Excludes `self_id`.
pub(crate) fn extract_relation_ids(v: &serde_json::Value, self_id: u64) -> Vec<u64> {
    let mut out = Vec::new();
    let Some(rels) = v.get("relations").and_then(|r| r.as_array()) else {
        return out;
    };
    for rel in rels {
        let is_wi_link = rel
            .get("rel")
            .and_then(|r| r.as_str())
            .is_some_and(|r| r.starts_with("System.LinkTypes"));
        if !is_wi_link {
            continue;
        }
        let Some(id) = rel
            .get("url")
            .and_then(|u| u.as_str())
            .and_then(|u| u.rsplit('/').next())
            .and_then(|s| s.parse::<u64>().ok())
        else {
            continue;
        };
        if id != self_id && !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// Standard base64 (RFC 4648) for the Basic-auth header — avoids pulling
/// the `base64` crate into the direct dependency tree for one call site.
pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(chunk.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Minimal HTML-to-text for ADO rich-text fields.
pub(crate) fn strip_html(html: &str) -> String {
    use std::sync::LazyLock;
    static BR: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?i)<br\s*/?>|</(?:p|div|li|tr)>").expect("valid"));
    static TAG: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"<[^>]+>").expect("valid"));
    let s = BR.replace_all(html, "\n");
    let s = TAG.replace_all(&s, "");
    s.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .trim()
        .to_string()
}

/// Extract (section, file) obligations from a get_change_set dossier's
/// text: every project-relative source-file reference inside a markdown
/// section, attributed to that section's heading. Pure text-level —
/// works on any dossier the agent was handed (including leak-free eval
/// snapshots), no emission-format coupling.
pub(crate) fn extract_dossier_obligations(dossier: &str) -> Vec<(String, String)> {
    use std::sync::LazyLock;
    // Project-relative paths with a source-ish extension; excludes bare
    // filenames (must contain a '/') so prose words don't match.
    static FILE_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
            r"[\w~][\w.~-]*(?:/[\w.~-]+)+\.(?:aspx\.vb|ascx\.vb|asax\.vb|master\.vb|vb|cs|ts|tsx|js|jsx|sql|resx|aspx|ascx|master|vbhtml|cshtml|config|dbml|css)\b",
        )
        .expect("valid regex")
    });
    let mut out: Vec<(String, String)> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut section = "dossier".to_string();
    for line in dossier.lines() {
        let t = line.trim_start();
        if let Some(h) = t.strip_prefix("## ") {
            section = h.trim().chars().take(60).collect();
            continue;
        }
        for m in FILE_RE.find_iter(line) {
            let f = m.as_str().to_string();
            if seen.insert(f.to_lowercase()) {
                out.push((section.clone(), f));
            }
        }
    }
    out
}

#[cfg(test)]
mod work_item_tests {
    use super::{
        ado_coords_from_remote_url, base64_encode, extract_work_item_id, parse_origin_url,
        pick_pat, story_for_concepts, strip_html,
    };

    #[test]
    fn concept_extraction_view_drops_injected_labels() {
        let s = "Bug #847\n\n## Work item (full text)\n[Bug #847] Can't assign resources \
                 to tasks in multitenant mode\n\nAcceptance criteria:\nnone";
        let cleaned = story_for_concepts(s);
        // The injected scaffolding labels are gone…
        assert!(!cleaned.contains("Work item (full text)"));
        assert!(!cleaned.contains("Acceptance criteria:"));
        // …while the actual story/work-item content survives verbatim.
        assert!(cleaned.contains("Can't assign resources to tasks in multitenant mode"));
        assert!(cleaned.contains("Bug #847"));
    }

    #[test]
    fn ado_coords_from_all_remote_shapes() {
        let ok = |u: &str| ado_coords_from_remote_url(u).expect(u);
        assert_eq!(
            ok("https://dev.azure.com/exampleorg/ExampleRepo/_git/ExampleRepo"),
            ("exampleorg".into(), "ExampleRepo".into())
        );
        // user@ prefix (credential-embedding clone URLs) and trailing slash.
        assert_eq!(
            ok("https://exampleuser@dev.azure.com/exampleorg/ExampleRepo/_git/ExampleRepo/"),
            ("exampleorg".into(), "ExampleRepo".into())
        );
        assert_eq!(
            ok("git@ssh.dev.azure.com:v3/myorg/My%20Project/repo"),
            ("myorg".into(), "My%20Project".into())
        );
        assert_eq!(
            ok("https://myorg.visualstudio.com/proj/_git/repo"),
            ("myorg".into(), "proj".into())
        );
        assert_eq!(
            ok("https://myorg.visualstudio.com/DefaultCollection/proj/_git/repo"),
            ("myorg".into(), "proj".into())
        );
        // Non-ADO remotes and malformed paths → None, never a wrong guess.
        assert!(ado_coords_from_remote_url("https://github.com/org/repo.git").is_none());
        assert!(ado_coords_from_remote_url("https://dev.azure.com/org").is_none());
        assert!(ado_coords_from_remote_url("https://dev.azure.com/org/proj/notgit/x").is_none());
        assert!(ado_coords_from_remote_url("").is_none());
    }

    #[test]
    fn origin_url_parsed_from_git_config() {
        let cfg = r#"[core]
	repositoryformatversion = 0
[remote "upstream"]
	url = https://example.com/other.git
[remote "origin"]
	url = https://dev.azure.com/exampleorg/ExampleRepo/_git/ExampleRepo
	fetch = +refs/heads/*:refs/remotes/origin/*
"#;
        assert_eq!(
            parse_origin_url(cfg).as_deref(),
            Some("https://dev.azure.com/exampleorg/ExampleRepo/_git/ExampleRepo")
        );
        // No origin section → None (never the wrong remote's URL).
        assert!(parse_origin_url("[remote \"upstream\"]\n\turl = https://x/y").is_none());
    }

    #[test]
    fn concept_view_drops_url_tokens() {
        let s = "Camera icon bug\n\nSee https://support.example.com/agent/tickets/956 for repro";
        let cleaned = story_for_concepts(s);
        assert!(!cleaned.contains("support.example"), "{cleaned}");
        assert!(!cleaned.contains("https://"), "{cleaned}");
        assert!(cleaned.contains("Camera icon bug"));
        assert!(cleaned.contains("for repro"));
    }

    #[test]
    fn relation_ids_extracted_from_worklinks_only() {
        use super::extract_relation_ids;
        let v = serde_json::json!({
            "relations": [
                {"rel": "System.LinkTypes.Related", "url": "https://dev.azure.com/o/p/_apis/wit/workItems/817"},
                {"rel": "System.LinkTypes.Hierarchy-Reverse", "url": ".../workItems/100"},
                {"rel": "AttachedFile", "url": ".../attachments/abc"},
                {"rel": "ArtifactLink", "url": "vstfs:///Git/Commit/xyz"},
                {"rel": "System.LinkTypes.Related", "url": ".../workItems/691"}
            ]
        });
        // self (691) excluded; attachments/artifacts ignored; order kept.
        assert_eq!(extract_relation_ids(&v, 691), vec![817, 100]);
        assert_eq!(
            extract_relation_ids(&serde_json::json!({}), 1),
            Vec::<u64>::new()
        );
    }

    #[test]
    fn pat_precedence_per_call_then_env_then_none() {
        // Per-call PAT wins over the env fallback.
        assert_eq!(
            pick_pat(Some("call-pat".into()), Some("env-pat".into())),
            Some("call-pat".to_string())
        );
        // Blank per-call falls through to env (a live agent passes nothing).
        assert_eq!(
            pick_pat(Some("  ".into()), Some("env-pat".into())),
            Some("env-pat".to_string())
        );
        assert_eq!(
            pick_pat(None, Some(" env-pat \n".into())),
            Some("env-pat".to_string())
        );
        // Neither source → auto-fetch silently degrades.
        assert_eq!(pick_pat(None, None), None);
        assert_eq!(pick_pat(Some(String::new()), Some("".into())), None);
    }

    #[test]
    fn extracts_ids_from_common_forms() {
        assert_eq!(extract_work_item_id("Bug #847: Can't assign"), Some(847));
        assert_eq!(
            extract_work_item_id("fix bug 847 in tenant mode"),
            Some(847)
        );
        assert_eq!(extract_work_item_id("US 1234 as a user I want"), Some(1234));
        assert_eq!(extract_work_item_id("AB#847 regression"), Some(847));
        assert_eq!(extract_work_item_id("resolves #55"), Some(55));
        assert_eq!(extract_work_item_id("supports 7 languages"), None);
        assert_eq!(extract_work_item_id("no ids here"), None);
    }

    #[test]
    fn base64_matches_rfc_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b":secret-pat"), "OnNlY3JldC1wYXQ=");
    }

    #[test]
    fn strip_html_flattens_ado_richtext() {
        let h = "<div>Issue</div><p>In multi-tenant mode, resources &amp; tasks<br/>fail.</p>";
        let t = strip_html(h);
        assert!(t.contains("Issue"));
        assert!(t.contains("resources & tasks"));
        assert!(!t.contains('<'));
    }
}

#[cfg(test)]
mod dossier_obligation_tests {
    use super::extract_dossier_obligations;

    #[test]
    fn extracts_files_per_section_dedup_case_insensitive() {
        let dossier = "\
# Change set\n\
## Co-change partners\n\
- `Site/App_Code/ata/code/huvud.vb` (21 co-changes)\n\
- `Site/App_GlobalResources/label.resx` family\n\
## History/log tables\n\
accessor: Site/App_Code/installationsobjekt/code/io-iom-log.vb\n\
## Notes\n\
prose without paths; duplicate Site/App_Code/ata/code/HUVUD.VB ignored\n";
        let obs = extract_dossier_obligations(dossier);
        let files: Vec<&str> = obs.iter().map(|(_, f)| f.as_str()).collect();
        assert_eq!(
            files,
            vec![
                "Site/App_Code/ata/code/huvud.vb",
                "Site/App_GlobalResources/label.resx",
                "Site/App_Code/installationsobjekt/code/io-iom-log.vb",
            ]
        );
        assert_eq!(obs[0].0, "Co-change partners");
        assert_eq!(obs[2].0, "History/log tables");
    }

    #[test]
    fn bare_filenames_and_prose_do_not_match() {
        let obs = extract_dossier_obligations("edit huvud.vb and the map page\nno paths here");
        assert!(
            obs.is_empty(),
            "bare filenames without '/' must not count: {obs:?}"
        );
    }
}

// ── get_gis_inventory ────────────────────────────────────────────────────────

/// One grouped row of spatial-call usage: (library, map class, modern
/// equivalent, call-site count, distinct source files).
pub(crate) type GisUsageRow = (
    String,
    String,
    String,
    usize,
    std::collections::BTreeSet<String>,
);

/// Group raw spatial-call rows (library, class, modern_equivalent, file,
/// call-site count) into per-(library, class) usage rows sorted by count.
/// The count comes from the extractor's `count` edge metadata (ingest
/// collapses duplicate edge keys, so multiplicity rides in metadata).
pub(crate) fn group_spatial_calls(
    rows: &[(String, String, String, String, usize)],
) -> Vec<GisUsageRow> {
    let mut grouped: BTreeMap<
        (String, String),
        (String, usize, std::collections::BTreeSet<String>),
    > = BTreeMap::new();
    for (lib, class, modern, file, count) in rows {
        let e = grouped
            .entry((lib.clone(), class.clone()))
            .or_insert_with(|| (modern.clone(), 0, Default::default()));
        e.1 += (*count).max(1);
        if !file.is_empty() {
            e.2.insert(file.clone());
        }
    }
    let mut out: Vec<GisUsageRow> = grouped
        .into_iter()
        .map(|((lib, class), (modern, count, files))| (lib, class, modern, count, files))
        .collect();
    out.sort_by(|a, b| {
        b.3.cmp(&a.3)
            .then_with(|| a.0.cmp(&b.0))
            .then_with(|| a.1.cmp(&b.1))
    });
    out
}

impl Engram {
    /// GIS surface inventory: which map libraries/classes the project uses and
    /// where, per-file map configurations (api key / zoom / center), and the
    /// WMS/XYZ/Esri layer inventory — the map-stack documentation an agent
    /// needs before touching any map feature.
    pub async fn handle_get_gis_inventory(
        &self,
        req: crate::models::ProjectIdRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let _ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;

        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let (configs, layers, usage, spatial_total) = tokio::task::spawn_blocking(move || {
            let nodes = graph
                .query_nodes(&pid, None, None, None, crate::handlers::NODE_SCAN_LIMIT)
                .unwrap_or_default();

            // Per-file map configuration facts ("gis_config:{file}:{kind}").
            let mut configs: BTreeMap<String, Vec<String>> = BTreeMap::new();
            // Layer inventory from layer_type metadata: (type, name, file).
            let mut layers: Vec<(String, String, String)> = Vec::new();
            for n in &nodes {
                if n.node_type == "gis_config"
                    && let Some(rest) = n.name.strip_prefix("gis_config:")
                    && let Some((file, kind)) = rest.rsplit_once(':')
                {
                    configs
                        .entry(file.to_string())
                        .or_default()
                        .push(kind.to_string());
                }
                if let Some(lt) = n
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("layer_type"))
                    .and_then(|v| v.as_str())
                {
                    layers.push((
                        lt.to_string(),
                        n.name.clone(),
                        n.file_path.as_str().to_string(),
                    ));
                }
            }
            layers.sort();
            layers.dedup();

            // Spatial call edges → (library, class, modern_equivalent, file).
            let spatial = graph
                .list_edges_by_kind(&pid, EdgeKind::SpatialCall, 100_000)
                .unwrap_or_default();
            let rows: Vec<(String, String, String, String, usize)> = spatial
                .iter()
                .map(|e| {
                    let m = e.metadata.as_ref();
                    let get = |k: &str| {
                        m.and_then(|m| m.get(k))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string()
                    };
                    let count = m
                        .and_then(|m| m.get("count"))
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.parse::<usize>().ok())
                        .unwrap_or(1);
                    let file = e
                        .source_id
                        .strip_prefix("file:")
                        .unwrap_or(&e.source_id)
                        .to_string();
                    (
                        get("gis_library"),
                        get("map_class"),
                        get("modern_equivalent"),
                        file,
                        count,
                    )
                })
                .collect();
            // Edges without gis_library metadata are config/layer references
            // (file → gis_config node), not API call sites — the configs and
            // layer sections already cover them.
            let rows: Vec<_> = rows.into_iter().filter(|r| !r.0.is_empty()).collect();
            let spatial_total: usize = rows.iter().map(|r| r.4.max(1)).sum();
            (configs, layers, group_spatial_calls(&rows), spatial_total)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if configs.is_empty() && layers.is_empty() && usage.is_empty() {
            let mut out = String::from(
                "No GIS surface detected (no gis_config nodes, layer metadata, or spatial_call \
                 edges). If this project uses maps, its library may not be covered by the GIS \
                 extractors yet.",
            );
            out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
            return Ok(CallToolResult::success(vec![Content::text(out)]));
        }

        let mut out = String::from("# GIS surface inventory\n");

        if !usage.is_empty() {
            out.push_str(&format!(
                "\n## Map API usage ({spatial_total} call sites)\n"
            ));
            for (lib, class, modern, count, files) in usage.iter().take(30) {
                out.push_str(&format!(
                    "- {lib}.{class}: {count} call site(s) in {} file(s)",
                    files.len()
                ));
                if !modern.is_empty() {
                    out.push_str(&format!(" — modern equivalent: {modern}"));
                }
                out.push('\n');
                for f in files.iter().take(3) {
                    out.push_str(&format!("    {f}\n"));
                }
                if files.len() > 3 {
                    out.push_str(&format!("    ... and {} more file(s)\n", files.len() - 3));
                }
            }
            if usage.len() > 30 {
                out.push_str(&format!("  ... and {} more API(s)\n", usage.len() - 30));
            }
        }

        if !configs.is_empty() {
            out.push_str(&format!(
                "\n## Map configurations ({} file(s))\n",
                configs.len()
            ));
            for (file, kinds) in configs.iter().take(20) {
                let mut ks = kinds.clone();
                ks.sort();
                ks.dedup();
                let key_flag = if ks.iter().any(|k| k == "api_key") {
                    " — note: API key referenced in client code"
                } else {
                    ""
                };
                out.push_str(&format!("- {file}: {}{}\n", ks.join(", "), key_flag));
            }
            if configs.len() > 20 {
                out.push_str(&format!("  ... and {} more\n", configs.len() - 20));
            }
        }

        if !layers.is_empty() {
            out.push_str(&format!("\n## Layer inventory ({})\n", layers.len()));
            for (lt, name, file) in layers.iter().take(25) {
                out.push_str(&format!("- [{lt}] {name} ({file})\n"));
            }
            if layers.len() > 25 {
                out.push_str(&format!("  ... and {} more\n", layers.len() - 25));
            }
        }

        out.push_str(
            "\nnext: get_concept_footprint(concept=\"map\"/\"layer\") for full touchpoints; \
             find_implementation_pattern(pattern=\"map init\") for the house style; \
             blast_radius on the map config class before changing layer settings.\n",
        );
        out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }
}

#[cfg(test)]
mod gis_inventory_tests {
    use super::group_spatial_calls;

    fn row(
        lib: &str,
        class: &str,
        modern: &str,
        file: &str,
        count: usize,
    ) -> (String, String, String, String, usize) {
        (lib.into(), class.into(), modern.into(), file.into(), count)
    }

    #[test]
    fn groups_by_library_and_class_summing_counts() {
        let rows = vec![
            // a.js has 5 Polygon call sites collapsed into one edge with count=5
            row("google_maps", "Polygon", "MapLibre fill layer", "a.js", 5),
            row("google_maps", "Polygon", "MapLibre fill layer", "b.js", 1),
            row("leaflet", "TileLayer", "MapLibre raster source", "c.js", 1),
        ];
        let grouped = group_spatial_calls(&rows);
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[0].0, "google_maps");
        assert_eq!(grouped[0].1, "Polygon");
        assert_eq!(grouped[0].3, 6, "5 + 1 call sites");
        assert_eq!(grouped[0].4.len(), 2, "two distinct files");
        assert_eq!(grouped[1].0, "leaflet");
    }

    #[test]
    fn zero_count_clamps_to_one_and_empty_inputs_handled() {
        assert!(group_spatial_calls(&[]).is_empty());
        let grouped = group_spatial_calls(&[row("esri", "FeatureLayer", "", "", 0)]);
        assert_eq!(grouped[0].3, 1, "count 0 clamps to 1");
        assert!(grouped[0].4.is_empty(), "empty file string is dropped");
    }
}

#[cfg(test)]
mod unwired_tests {
    use super::unwired_should_flag;

    #[test]
    fn zero_caller_ordinary_methods_are_flagged() {
        assert!(unwired_should_flag("Helper", false, 0));
        assert!(unwired_should_flag("DataAccess", false, 0));
    }

    #[test]
    fn framework_invoked_kinds_are_excluded() {
        for kind in ["Lifecycle", "ControlEvent", "WebMethod"] {
            assert!(
                !unwired_should_flag(kind, false, 0),
                "{kind} is framework-invoked — zero static callers is expected"
            );
        }
    }

    #[test]
    fn handles_clause_methods_are_excluded() {
        assert!(
            !unwired_should_flag("Helper", true, 0),
            "Handles-bound methods are invoked by events, not code"
        );
    }

    #[test]
    fn methods_with_callers_are_not_flagged() {
        assert!(!unwired_should_flag("Helper", false, 1));
        assert!(!unwired_should_flag("DataAccess", false, 3));
    }

    /// Regression: call edges from the Roslyn path are stored as
    /// `EdgeKind::Calls`, not `Dependency`. Every caller-count consumer
    /// (this unwired filter, find_dead_methods, check_edit_safety,
    /// get_method_info) counts through `incoming_caller_edges` — a method
    /// whose ONLY incoming edge is a `Calls` edge must be reported as
    /// having callers, not as dead/unwired scaffolding.
    #[test]
    fn calls_only_edges_count_as_callers() {
        use crate::handlers::incoming_caller_edges;
        use engram_graph::{Edge, EdgeKind, GraphStore};

        fn edge(src: &str, tgt: &str, kind: EdgeKind) -> Edge {
            Edge {
                source_id: src.into(),
                target_id: tgt.into(),
                namespace: "memory".into(),
                language: "vb".into(),
                edge_kind: kind,
                weight: 1,
                generation: 1,
                metadata: None,
                updated_at_ms: 0,
            }
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let store = GraphStore::open(&tmp.path().join("graph.redb")).expect("open graph");
        let pid = "callers-regression";

        // A method whose only incoming edge is Calls (Roslyn call graph).
        store
            .upsert_edges(pid, &[edge("sym:caller", "sym:callee", EdgeKind::Calls)])
            .expect("upsert Calls edge");

        // Exactly how the unwired filter and find_dead_methods count (limit 1).
        let caller_count = incoming_caller_edges(&store, pid, "sym:callee", 1).len();
        assert_eq!(
            caller_count, 1,
            "a Calls-only incoming edge must count as a caller"
        );
        assert!(
            !unwired_should_flag("Helper", false, caller_count),
            "a method with a Calls caller is wired — must not be flagged"
        );
        assert_ne!(
            caller_count, 0,
            "find_dead_methods' zero-caller predicate must see the method as alive"
        );

        // Dependency-only callers still count (pre-existing behavior kept).
        store
            .upsert_edges(
                pid,
                &[edge(
                    "sym:dep_caller",
                    "sym:dep_callee",
                    EdgeKind::Dependency,
                )],
            )
            .expect("upsert Dependency edge");
        assert_eq!(
            incoming_caller_edges(&store, pid, "sym:dep_callee", 10).len(),
            1,
            "Dependency-only incoming edge must still count as a caller"
        );

        // The same caller carrying BOTH kinds deduplicates to one entry.
        store
            .upsert_edges(
                pid,
                &[
                    edge("sym:dual", "sym:both_callee", EdgeKind::Calls),
                    edge("sym:dual", "sym:both_callee", EdgeKind::Dependency),
                ],
            )
            .expect("upsert dual-kind edges");
        assert_eq!(
            incoming_caller_edges(&store, pid, "sym:both_callee", 10).len(),
            1,
            "same source with Calls + Dependency edges is one caller, not two"
        );
    }
}

#[cfg(test)]
mod agent_integration_tests {
    //! The generated integration pack is agent-facing text: every
    //! `tool_name(param=...)` it mentions is executed verbatim by an agent.
    //! A mention that names a field the request schema rejects
    //! (`deny_unknown_fields`) is a mandated call that can never succeed.
    //! These tests bind the generated text to the live tool registry.
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    /// tool name -> top-level input-schema property names, from the same
    /// router the MCP server advertises.
    fn schema_props() -> BTreeMap<String, BTreeSet<String>> {
        crate::tools::Engram::tool_registry()
            .list_all()
            .into_iter()
            .map(|t| {
                let props: BTreeSet<String> = t
                    .input_schema
                    .get("properties")
                    .and_then(|p| p.as_object())
                    .map(|o| o.keys().cloned().collect())
                    .unwrap_or_default();
                (t.name.to_string(), props)
            })
            .collect()
    }

    /// Every `snake_case_tool(snake_param=` mention that is NOT a real tool,
    /// or names a param the tool's schema does not declare.
    fn contract_violations(text: &str) -> Vec<String> {
        let re = regex::Regex::new(r"\b([a-z][a-z0-9_]+)\(([a-z][a-z0-9_]*)=").unwrap();
        let props = schema_props();
        assert!(props.len() > 100, "router enumerated {} tools", props.len());
        let mut bad = Vec::new();
        for c in re.captures_iter(text) {
            let tool = &c[1];
            let param = &c[2];
            match props.get(tool) {
                None => bad.push(format!("`{tool}(` is not a registered tool")),
                Some(p) if !p.contains(param) => bad.push(format!(
                    "`{tool}({param}=` names a field the schema rejects; declared: {:?}",
                    p.iter().collect::<Vec<_>>()
                )),
                _ => {}
            }
        }
        bad.sort();
        bad.dedup();
        bad
    }

    #[test]
    fn workflow_rules_only_name_real_tools_and_fields() {
        let rules = render_workflow_rules("pid-1", "C:/repo");
        let bad = contract_violations(&rules);
        assert!(
            bad.is_empty(),
            "workflow rules violate tool schemas:\n{}",
            bad.join("\n")
        );
    }

    #[test]
    fn hooks_only_name_real_tools_and_fields() {
        for windows in [true, false] {
            let hooks = render_hooks_json(windows);
            let bad = contract_violations(&hooks);
            assert!(
                bad.is_empty(),
                "hooks (windows={windows}) violate tool schemas:\n{}",
                bad.join("\n")
            );
        }
    }

    #[test]
    fn planning_briefs_only_name_real_tools_and_fields() {
        for (label, text) in [
            ("concept footprint next-steps", NEXT_STEPS_CONCEPT_FOOTPRINT),
            ("plan_user_story checklist", STORY_CHECKLIST),
        ] {
            let bad = contract_violations(text);
            assert!(
                bad.is_empty(),
                "{label} violates tool schemas:\n{}",
                bad.join("\n")
            );
        }
    }

    #[test]
    fn workflow_rules_report_the_registered_gate_count() {
        let rules = render_workflow_rules("pid-1", "C:/repo");
        let n = crate::services::pre_commit_review_service::gates::all_gates().len();
        assert!(
            !rules.contains("eleven gates"),
            "gate count is hardcoded prose; registry has {n}"
        );
        assert!(
            rules.contains(&format!("{n} gates")),
            "rules must state the registered gate count ({n}); got:\n{rules}"
        );
    }

    #[test]
    fn workflow_rules_tell_the_agent_how_to_recover_a_stale_project_id() {
        // The id is baked in at generation time; a reindex under a new id
        // (2026-07-19 data_dir reset) left every generated call failing.
        let rules = render_workflow_rules("pid-1", "C:/repo/ociusx");
        assert!(
            rules.contains("list_projects"),
            "no recovery path named:\n{rules}"
        );
        assert!(
            rules.contains("C:/repo/ociusx"),
            "recovery must key on the indexed directory so the agent can match it:\n{rules}"
        );
    }

    #[test]
    fn agents_md_names_project_and_passes_contract() {
        let md = render_agents_md("pid-1", "C:/repo");
        assert!(md.contains("pid-1"));
        assert!(md.contains("Engram"));
        let bad = contract_violations(&md);
        assert!(
            bad.is_empty(),
            "AGENTS.md violates tool schemas:\n{}",
            bad.join("\n")
        );
    }

    #[test]
    fn mcp_json_snippet_registers_engram_as_stdio_server() {
        let snippet = render_mcp_json_snippet(
            std::path::Path::new("C:/bin/engram_server.exe"),
            Some("C:/cfg/engram_mcp.yaml"),
        );
        let v: serde_json::Value = serde_json::from_str(&snippet).expect("valid JSON");
        let engram = &v["mcpServers"]["engram"];
        assert_eq!(engram["command"], "C:/bin/engram_server.exe");
        assert_eq!(
            engram["env"]["ENGRAM_CONFIG_PATH"],
            "C:/cfg/engram_mcp.yaml"
        );
        let without_cfg =
            render_mcp_json_snippet(std::path::Path::new("/usr/bin/engram_server"), None);
        let v: serde_json::Value = serde_json::from_str(&without_cfg).unwrap();
        assert!(v["mcpServers"]["engram"].get("env").is_none());
    }
}

#[cfg(test)]
mod change_set_rows_tests {
    use super::*;

    #[test]
    fn tail_cap_cuts_are_reported_as_omissions_not_dropped() {
        // 20 weak (concept-only, tier 3) server files + 1 golden co-change
        // file: the cap keeps 18 weak ones, reports 2, never touches golden.
        let mut prov: BTreeMap<String, BTreeSet<&'static str>> = BTreeMap::new();
        for i in 0..20 {
            prov.insert(
                format!("site/app_code/weak{i:02}.vb"),
                BTreeSet::from(["concept"]),
            );
        }
        prov.insert(
            "site/app_code/golden.vb".into(),
            BTreeSet::from(["cochange"]),
        );
        let (rows, omissions) = change_set_rows(&prov);
        assert_eq!(rows.len(), 21);
        assert_eq!(omissions.len(), 2, "{omissions:?}");
        assert!(omissions.iter().all(|o| o.reason.contains("tail cap")));
        assert!(rows.iter().filter(|r| !r.omitted).count() == 19);
        assert!(
            !rows
                .iter()
                .any(|r| r.path.ends_with("golden.vb") && r.omitted)
        );
        // family / vtop rows are exempt from the cap
        prov.insert(
            "site/app_code/res.en.resx".into(),
            BTreeSet::from(["concept", "family"]),
        );
        let (rows, _) = change_set_rows(&prov);
        assert!(!rows.iter().any(|r| r.path.ends_with(".resx") && r.omitted));
        assert!(
            rows.iter()
                .any(|r| r.path.ends_with(".resx") && !r.signals.contains(&"family"))
        );
    }
}

#[cfg(test)]
mod story_concept_resolution_tests {
    //! Row-1 slice 4 / A1: concepts are ENTITIES resolved against the index,
    //! not the first three acceptable words. Additive: the three document-
    //! order words are kept (the eval-validated recipe), the new ones are
    //! appended when the index corroborates them.
    use super::*;

    const STORY: &str = "As an admin I want to set a main reporting category (huvudredovisningskategori) \
                         on a production code list category so that time reports roll up to it";

    #[test]
    fn candidates_include_parenthesized_domain_terms_and_noun_phrases() {
        let c = extract_story_concept_candidates(STORY);
        assert!(
            c.iter().any(|x| x == "huvudredovisningskategori"),
            "a parenthesized gloss is the author naming the domain entity: {c:?}"
        );
        assert!(
            c.iter().any(|x| x == "reporting category"),
            "adjacent non-stopword pairs are noun-phrase candidates: {c:?}"
        );
        assert!(
            c.iter()
                .any(|x| x == "code list category" || x == "list category"),
            "{c:?}"
        );
        // The document-order words are still there, first.
        assert_eq!(&c[..3], &["main", "reporting", "category"]);
    }

    #[test]
    fn resolution_keeps_only_index_corroborated_candidates_and_splits_compounds() {
        let index = vec![
            "Site/App_Code/redovisning/code/redovisningskategorier.vb".to_string(),
            "Site/modules/dashboard/pages/admin/production/productioncodelistcategory.aspx.vb"
                .to_string(),
            "db-ociusx.sql/dbo/Tables/rk_redovisningskategorier.sql".to_string(),
        ];
        let cands = vec![
            "main".to_string(),
            "reporting".to_string(),
            "category".to_string(),
            "huvudredovisningskategori".to_string(),
            "reporting category".to_string(),
            "code list category".to_string(),
            "unicorn".to_string(),
        ];
        let resolved = resolve_story_concepts(&cands, &index, 6);
        // The first three are never dropped (no regression on the recipe).
        assert_eq!(&resolved[..3], &["main", "reporting", "category"]);
        assert!(
            resolved.iter().any(|x| x == "redovisningskategori"),
            "the Swedish compound must resolve to the indexed stem (suffix split): {resolved:?}"
        );
        assert!(
            resolved
                .iter()
                .any(|x| x == "codelistcategory" || x == "code list category"),
            "a noun phrase whose compact form names a file is corroborated: {resolved:?}"
        );
        assert!(!resolved.iter().any(|x| x == "unicorn"), "{resolved:?}");
        assert!(resolved.len() <= 6);
    }

    #[test]
    fn resolution_without_index_evidence_is_the_plain_recipe() {
        let cands = vec![
            "main".into(),
            "reporting".into(),
            "category".into(),
            "zzz qqq".into(),
        ];
        let resolved = resolve_story_concepts(&cands, &[], 6);
        assert_eq!(resolved, vec!["main", "reporting", "category"]);
    }
}

#[cfg(test)]
mod change_set_tier_tests {
    //! Row-1 audit A2: rank by evidence DIRECTNESS, not signal count. A precise
    //! concept (entity) match outranks two weak associative signals.
    use super::*;

    fn t(sigs: &[&'static str]) -> u8 {
        change_set_tier(&sigs.iter().copied().collect::<BTreeSet<&'static str>>())
    }

    #[test]
    fn golden_signals_stay_on_top() {
        assert_eq!(t(&["cochange", "concept"]), 0);
        assert_eq!(t(&["history"]), 1);
        assert!(t(&["history"]) < t(&["concept", "vector"]));
    }

    #[test]
    fn concept_plus_weak_beats_concept_alone_which_beats_weak_pairs() {
        assert!(t(&["concept", "vector"]) < t(&["concept"]));
        assert!(
            t(&["concept"]) < t(&["vector", "graph"]),
            "two associative signals must not outrank an entity match: concept={} vector+graph={}",
            t(&["concept"]),
            t(&["vector", "graph"])
        );
        assert!(t(&["vector", "graph"]) <= t(&["vector"]));
    }
}

#[cfg(test)]
mod footprint_coverage_tests {
    //! Row-4 audit A1/A3/A7: the footprint pages the lexical index instead of
    //! stopping at 50 hits, expands ALL matching anchors with a per-anchor
    //! cap+1, and reports every cap and failure in a coverage block.
    use super::*;

    #[test]
    fn lexical_paging_reports_completeness_from_the_extra_hit() {
        assert_eq!(footprint_lexical_status(12, 2000), "complete");
        assert_eq!(footprint_lexical_status(2000, 2000), "complete");
        assert_eq!(footprint_lexical_status(2001, 2000), "truncated");
    }

    #[test]
    fn coverage_block_names_every_provider_and_cap() {
        let cov = FootprintCoverage {
            node_scan: "complete".into(),
            anchors_matched: 7,
            anchors_used: 7,
            anchor_cap: 50,
            consumers: "truncated".into(),
            consumer_edges: 401,
            consumer_cap_per_anchor: 200,
            lexical: "truncated".into(),
            lexical_files: 45,
            lexical_hits: 2001,
            lexical_page: 2000,
            failures: vec!["lexical search failed: boom".into()],
            ..Default::default()
        };
        let md = render_footprint_coverage(&cov);
        assert!(md.starts_with("\n## Coverage\n"), "{md}");
        for needle in [
            "- node scan: complete",
            "- anchors: 7 matched, 7 expanded (cap 50)",
            "- consumers: truncated (401 edges; per-anchor cap 200",
            "- lexical: truncated (45 files from 2001 hits; page 2000",
            "- failures: lexical search failed: boom",
        ] {
            assert!(md.contains(needle), "missing {needle:?} in:\n{md}");
        }
    }

    #[test]
    fn anchors_are_no_longer_capped_at_five() {
        // Ten matching tables: all ten are anchors (bounded by ANCHOR_CAP).
        let matched: Vec<(String, String)> = (0..10)
            .map(|i| (format!("table:t{i}"), format!("t{i}")))
            .collect();
        let (used, truncated) = footprint_select_anchors(&matched);
        assert_eq!(used.len(), 10);
        assert!(!truncated);
        let many: Vec<(String, String)> = (0..ANCHOR_CAP + 3)
            .map(|i| (format!("table:t{i}"), format!("t{i}")))
            .collect();
        let (used, truncated) = footprint_select_anchors(&many);
        assert_eq!(used.len(), ANCHOR_CAP);
        assert!(truncated);
    }
}

#[cfg(test)]
mod footprint_literal_tests {
    //! Row-4 audit A2: the footprint runs a LITERAL (substring, case-
    //! insensitive) pass over the indexed chunk text, because the tokenized
    //! index cannot see a stem inside an identifier
    //! (`rk_redovisningskategorier`). Its caps and status are reported.
    use super::*;

    #[test]
    fn literal_status_is_complete_unless_the_match_cap_was_filled() {
        assert_eq!(footprint_literal_status(0, 5000), "complete");
        assert_eq!(footprint_literal_status(4999, 5000), "complete");
        assert_eq!(footprint_literal_status(5000, 5000), "truncated");
    }

    #[test]
    fn coverage_block_reports_the_literal_pass() {
        let cov = FootprintCoverage {
            node_scan: "complete".into(),
            literal: "truncated".into(),
            literal_files: 25,
            literal_matches: 5000,
            literal_cap: 5000,
            ..Default::default()
        };
        let md = render_footprint_coverage(&cov);
        assert!(
            md.contains("- literal: truncated (25 files from 5000 matches; cap 5000)"),
            "{md}"
        );
    }

    #[test]
    fn literal_files_are_merged_into_the_text_section_and_deduped() {
        let graph_files: HashSet<&str> = ["Site/App_Code/a.vb"].into_iter().collect();
        let lexical = vec![
            "Site/App_Code/b.vb".to_string(),
            "Site/App_Code/a.vb".to_string(),
        ];
        let literal = vec![
            "Site/App_Code/c.aspx.vb".to_string(),
            "Site/App_Code/b.vb".to_string(),
            "node_modules/x/y.js".to_string(),
        ];
        let merged = footprint_text_only_files(&graph_files, &lexical, &literal);
        assert_eq!(
            merged,
            vec!["Site/App_Code/b.vb", "Site/App_Code/c.aspx.vb"]
        );
    }
}
