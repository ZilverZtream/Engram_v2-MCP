//! Merged-work corpus: PR-level examples from Git history, not approval proof.
//!
//! The `history` namespace already indexes per-commit messages and per-file
//! diffs, but an agent asking "how was similar work done here?" needs the
//! PR-LEVEL story: title + the COMPLETE file cohort that shipped together +
//! which domain it touched. Azure DevOps and GitHub both stamp the PR
//! identity into the first-parent commit ("Merged PR 1234: …" /
//! "Merge pull request #1234 …"), so plain git is enough — no PAT needed
//! for the core corpus.
//!
//! Two tools:
//! - `ingest_merged_prs` — incremental (watermarked) walk of first-parent
//!   commits; one compact searchable doc per merged PR / change unit.
//! - `find_merged_work` — story/domain query → top-N merged-PR cards, each
//!   showing shipped file cohorts to inspect for applicable patterns.

use crate::handlers::validate_project_id;
use crate::tools::Engram;
use engram_core::{ContentHash, DocIdStr};
use engram_git::{GitWalker, history::MergeCommitPolicy};
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use std::collections::HashMap;
use std::sync::LazyLock;

fn primary_task_text(story: &str) -> &str {
    if story.contains(['"', '`']) {
        return story;
    }
    static CONTEXT_START: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"(?i)\s+(?:from|using|via|with|between)\s+")
            .expect("task context boundary")
    });
    CONTEXT_START
        .find(story)
        .map(|matched| &story[..matched.start()])
        .filter(|prefix| !prefix.trim().is_empty())
        .unwrap_or(story)
}

/// Literal task/title matching, not a semantic similarity claim. Preserve
/// query adjacency for phrase matches; stopwords cannot create new phrases.
fn exemplar_title_rank(story: &str, content: &str) -> (usize, usize, usize, usize) {
    fn words(text: &str) -> Vec<String> {
        let mut separated = String::with_capacity(text.len());
        let mut previous_lower = false;
        for c in text.chars() {
            if c.is_uppercase() && previous_lower {
                separated.push(' ');
            }
            previous_lower = c.is_lowercase() || c.is_numeric();
            separated.push(c);
        }
        separated
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(|w| {
                let word = w.to_lowercase();
                if !meaningful(&word) {
                    word
                } else if word.len() > 4 && word.ends_with("ies") {
                    format!("{}y", &word[..word.len() - 3])
                } else if word.len() > 3
                    && word.ends_with('s')
                    && !word.ends_with("ss")
                    && !word.ends_with("us")
                {
                    word[..word.len() - 1].to_string()
                } else {
                    word
                }
            })
            .collect()
    }
    fn meaningful(word: &str) -> bool {
        !matches!(
            word,
            "a" | "an"
                | "the"
                | "from"
                | "for"
                | "to"
                | "of"
                | "in"
                | "on"
                | "with"
                | "and"
                | "or"
                | "how"
                | "can"
                | "should"
                | "was"
                | "were"
                | "is"
                | "are"
                | "be"
                | "by"
                | "as"
                | "this"
                | "that"
        )
    }
    let mut title = content
        .lines()
        .next()
        .and_then(|s| s.split_once(':'))
        .map(|(_, s)| s)
        .unwrap_or("");
    // Conventional leading change classifications are metadata, not task
    // objects. Unknown/domain tags remain searchable title content.
    loop {
        title = title.trim_start_matches(|c: char| c.is_whitespace() || c == '+' || c == '-');
        let Some(rest) = title.strip_prefix('[') else {
            break;
        };
        let Some((tag, rest)) = rest.split_once(']') else {
            break;
        };
        let tag = tag.to_lowercase();
        let conventional = !tag.is_empty()
            && tag.split_whitespace().all(|word| {
                matches!(
                    word,
                    "feature"
                        | "change"
                        | "bug"
                        | "fix"
                        | "bugfix"
                        | "chore"
                        | "refactor"
                        | "improvement"
                        | "enhancement"
                        | "docs"
                        | "documentation"
                        | "test"
                        | "tests"
                        | "perf"
                        | "performance"
                ) || word.strip_prefix('p').is_some_and(|digits| {
                    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
                })
            });
        if !conventional {
            break;
        }
        title = rest;
    }
    let query = words(story);
    let title = words(title);
    let rank = |query: &[String]| {
        let terms: std::collections::HashSet<_> = query.iter().filter(|w| meaningful(w)).collect();
        let coverage = terms.iter().filter(|word| title.contains(word)).count();
        let phrases: std::collections::HashSet<_> = query
            .windows(2)
            .filter(|pair| meaningful(&pair[0]) && meaningful(&pair[1]))
            .collect();
        let phrase_matches = phrases
            .iter()
            .filter(|pair| title.windows(2).any(|t| t == **pair))
            .count();
        (phrase_matches, coverage)
    };
    // Bounded English heuristic: task before contextual clauses such as
    // "create purchase request FROM invoice rows". Quoted/identifier queries
    // are kept whole rather than reinterpreting a quoted preposition.
    let boundary = words(primary_task_text(story)).len();
    let boundary = if query[..boundary].iter().any(|word| meaningful(word)) {
        boundary
    } else {
        query.len()
    };
    let primary = rank(&query[..boundary]);
    let all = rank(&query);
    (primary.0, primary.1, all.0, all.1)
}

/// Parse a PR identity from a first-parent commit summary.
/// Returns (pr_id, title). Falls back to the short oid + full summary for
/// direct pushes so the corpus still covers repos without PR discipline.
pub(crate) fn parse_pr_identity(summary: &str, short_oid: &str) -> (String, String) {
    static ADO_RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?i)^merged pr (\d+)\s*:?\s*(.*)$").expect("ADO_RE"));
    static GH_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"(?i)^merge pull request #(\d+)(?:\s+from\s+\S+)?\s*(.*)$")
            .expect("GH_RE")
    });
    if let Some(c) = ADO_RE.captures(summary) {
        let title = c[2].trim().to_string();
        return (
            format!("PR-{}", &c[1]),
            if title.is_empty() {
                summary.to_string()
            } else {
                title
            },
        );
    }
    if let Some(c) = GH_RE.captures(summary) {
        let title = c[2].trim().to_string();
        return (
            format!("PR-{}", &c[1]),
            if title.is_empty() {
                summary.to_string()
            } else {
                title
            },
        );
    }
    (format!("commit-{short_oid}"), summary.to_string())
}

/// Coarse domain classification from file paths: the top-2 most common
/// directory prefixes (up to 4 segments, vendor-filtered). Deliberately
/// coarse — the goal is "admin/system/user work" not a taxonomy.
pub(crate) fn classify_domains(files: &[String]) -> Vec<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for f in files {
        let norm = f.replace('\\', "/").to_lowercase();
        if engram_core::is_vendor_path(&norm) {
            continue;
        }
        let mut segs: Vec<&str> = norm.split('/').collect();
        segs.pop(); // drop the filename
        if segs.is_empty() {
            continue;
        }
        let depth = segs.len().min(4);
        let key = segs[..depth].join("/");
        *counts.entry(key).or_default() += 1;
    }
    let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.into_iter().take(2).map(|(d, _)| d).collect()
}

/// Ultra-coarse change-kind taxonomy from file extensions/paths. Deliberately
/// broad buckets so an agent can filter exemplars by the SHAPE of its task —
/// "adding a button" (ui), "adding a table" (database), "adding a setting"
/// (settings) — without a curated taxonomy that would rot.
pub(crate) fn classify_kinds(files: &[String]) -> Vec<String> {
    let mut kinds: Vec<&'static str> = Vec::new();
    let add = |k: &'static str, kinds: &mut Vec<&'static str>| {
        if !kinds.contains(&k) {
            kinds.push(k);
        }
    };
    for f in files {
        let p = f.replace('\\', "/").to_lowercase();
        if engram_core::is_vendor_path(&p) {
            continue;
        }
        if p.ends_with(".aspx.vb")
            || p.ends_with(".ascx.vb")
            || p.ends_with(".aspx.cs")
            || p.ends_with(".ascx.cs")
        {
            add("ui-code", &mut kinds);
        } else if p.ends_with(".aspx")
            || p.ends_with(".ascx")
            || p.ends_with(".master")
            || p.ends_with(".html")
            || p.ends_with(".css")
        {
            add("ui-markup", &mut kinds);
        } else if p.ends_with(".js") || p.ends_with(".ts") || p.ends_with(".tsx") {
            add("js", &mut kinds);
        } else if p.ends_with(".sql") {
            add("database", &mut kinds);
        } else if p.ends_with(".config") || p.contains("settings") {
            add("settings", &mut kinds);
        } else if p.ends_with(".resx") {
            add("resources", &mut kinds);
        } else if p.ends_with(".vb") || p.ends_with(".cs") {
            if p.contains("/api") {
                add("api", &mut kinds);
            } else {
                add("backend", &mut kinds);
            }
        }
    }
    kinds.into_iter().map(str::to_string).collect()
}

/// Render the searchable per-PR doc. Kept compact: retrieval returns these
/// verbatim, so every line must earn its tokens.
pub(crate) fn render_pr_doc(
    pr_id: &str,
    title: &str,
    author: &str,
    timestamp: u64,
    body: &str,
    domains: &[String],
    files: &[String],
) -> String {
    let kinds = classify_kinds(files);
    let mut md = String::with_capacity(1024);
    md.push_str(&format!("# {pr_id}: {title}\n"));
    md.push_str(&format!(
        "merged: {} | author: {author} | files: {} | domains: {} | kinds: {}\n",
        crate::utils::ymd_utc(timestamp * 1000),
        files.len(),
        if domains.is_empty() {
            "-".to_string()
        } else {
            domains.join(", ")
        },
        if kinds.is_empty() {
            "-".to_string()
        } else {
            kinds.join(", ")
        }
    ));
    md.push_str("provenance: git commit contents; review approvals were not fetched\n");
    let trimmed_body: String = body.trim().chars().take(600).collect();
    if !trimmed_body.is_empty() && trimmed_body != title {
        md.push_str(&format!("\n{trimmed_body}\n"));
    }
    md.push_str("\n## Files shipped together in this change\n");
    for f in files {
        md.push_str(&format!("- {f}\n"));
    }
    md
}

/// Layer profile of a pr-doc `kinds:` value: (touches_client, touches_server).
/// Client = ui-code/ui-markup/js; server = backend/api/database. `settings`
/// and `resources` are layer-neutral (both sides ship them).
pub(crate) fn layer_profile(kinds_line: &str) -> (bool, bool) {
    let mut client = false;
    let mut server = false;
    for k in kinds_line.split(',').map(str::trim) {
        match k {
            "ui-code" | "ui-markup" | "js" => client = true,
            "backend" | "api" | "database" => server = true,
            _ => {}
        }
    }
    (client, server)
}

/// Epoch seconds at 00:00:00 UTC of a `YYYY-MM-DD` date (shape validated by
/// the caller; impossible day-of-month values are accepted like `date -u`
/// would normalize them — the callers only need a monotonic cutoff).
/// Hinnant's days_from_civil; no chrono dependency. Lets `merged_before`
/// cutoffs ride the indexed `timestamp` field INSIDE the query instead of
/// post-ranking display filtering — post-cutoff docs were eating the top_k
/// slots, so the survivors shifted whenever the corpus gained newer PRs
/// (live 2026-07-10: PR1913 replay picked different exemplars after a
/// corpus backfill added two months of PRs).
pub(crate) fn ymd_to_epoch_secs(ymd: &str) -> Option<u64> {
    let y: i64 = ymd.get(0..4)?.parse().ok()?;
    let m: i64 = ymd.get(5..7)?.parse().ok()?;
    let d: i64 = ymd.get(8..10)?.parse().ok()?;
    let max_day = match m {
        2 if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => return None,
    };
    if !(1..=max_day).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400).ok()
}

/// Compact view of a `pr:` history doc for embedding in a dossier. A plain
/// char-head is the WRONG cut here: the doc layout is title/meta → body
/// (≤600 chars) → file cohort, so a 500-char head usually ends before the
/// cohort — the one part that shows the SHAPE of an approved change (and
/// the part agents won't fetch via a follow-up call; utilization-wall
/// lesson). Keep the title + meta line, the first two body lines, and the
/// cohort capped at `max_files` with a folded overflow count.
pub(crate) fn exemplar_view(content: &str, max_files: usize) -> String {
    let mut out = String::new();
    let mut body_lines = 0usize;
    let mut in_cohort = false;
    let mut shown_files = 0usize;
    let mut extra_files = 0usize;
    for (i, line) in content.lines().enumerate() {
        if i == 0 || line.starts_with("merged: ") || line.starts_with("provenance: ") {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if line.starts_with("## Files shipped together") {
            in_cohort = true;
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if in_cohort {
            if line.starts_with("- ") {
                if shown_files < max_files {
                    out.push_str(line);
                    out.push('\n');
                    shown_files += 1;
                } else {
                    extra_files += 1;
                }
            } else if let Some(n) = line
                .strip_prefix("... and ")
                .and_then(|r| r.split_whitespace().next())
                .and_then(|s| s.parse::<usize>().ok())
            {
                // Fold the doc's own overflow marker into ours.
                extra_files += n;
            }
        } else if body_lines < 2 {
            // Body: first two CONTENT lines. Heading-only lines are PR
            // description-template artifacts ("###Task/work completed",
            // "## How to test") — labels, not prose; skip them.
            let t = line.trim();
            if !t.is_empty() && !t.starts_with('#') {
                out.push_str(line);
                out.push('\n');
                body_lines += 1;
            }
        }
    }
    if extra_files > 0 {
        out.push_str(&format!("... and {extra_files} more\n"));
    }
    out
}

impl Engram {
    pub async fn handle_ingest_merged_prs(
        &self,
        req: crate::models::IngestMergedPrsRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let rec = self.ensure_project_record(&req.project_id).await?;
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;
        let max_commits = req.max_commits.clamp(1, 20_000);
        // Leak-free cutoff (point-in-time eval snapshots): lexical ISO-date
        // comparison, the same convention find_merged_work uses query-side.
        let merged_before: Option<String> = req
            .merged_before
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(str::to_string);
        if let Some(d) = &merged_before
            && (d.len() != 10
                || ymd_to_epoch_secs(d).is_none()
                || !d.chars().enumerate().all(|(i, c)| {
                    if i == 4 || i == 7 {
                        c == '-'
                    } else {
                        c.is_ascii_digit()
                    }
                }))
        {
            return Err(McpError::invalid_params(
                format!("merged_before must be YYYY-MM-DD, got '{d}'"),
                None,
            ));
        }

        // Incremental: only walk commits newer than the watermark.
        let watermark_key = "pr_ingest_watermark";
        let reg = self.state.registry.clone();
        let pid = req.project_id.clone();
        let watermark: Option<String> =
            tokio::task::spawn_blocking(move || reg.get_meta(&pid, watermark_key).ok().flatten())
                .await
                .unwrap_or(None);
        // rebuild=true ignores the watermark: re-walk and re-render the whole
        // corpus (stable pr:<id> pks make this an in-place upsert). Needed
        // after doc-format/generation changes.
        let stop_oid = if req.rebuild {
            None
        } else {
            watermark
                .as_deref()
                .and_then(|s| git2::Oid::from_str(s).ok())
        };

        // Preserve the current corpus until the git walk and new indexing
        // succeed. A failed rebuild must not erase previously usable evidence.
        let previous_paths: std::collections::BTreeSet<String> = if req.rebuild {
            ps.search
                .list_docs_in_namespace(&req.project_id, engram_core::namespaces::NAMESPACE_HISTORY)
                .map_err(|error| McpError::internal_error(error.to_string(), None))?
                .into_iter()
                .filter(|doc| doc.path.starts_with("pr:"))
                .map(|doc| doc.path)
                .collect()
        } else {
            Default::default()
        };

        let repo_dir = std::path::PathBuf::from(&rec.directory);
        type PrUnit = (String, String, String, u64, String, Vec<String>);
        let cutoff = merged_before.clone();
        let (units, terminal, root_note): (Vec<PrUnit>, Option<String>, &'static str) =
            tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                let repo = GitWalker::open_repo(&repo_dir)?;
                let cancel = tokio_util::sync::CancellationToken::new();
                // Walk the REMOTE DEFAULT branch, not the checkout: this
                // corpus presents itself as merged work, and a
                // checked-out feature branch would leak in-flight commits
                // into it (observed live with an unmerged dialog PR).
                let root = GitWalker::approved_history_root(&repo);
                let root_note = if root.is_some() {
                    "origin default branch"
                } else {
                    "HEAD (no origin default branch found)"
                };
                let oids = GitWalker::walk_new_commits_from(
                    &repo,
                    root,
                    stop_oid,
                    max_commits,
                    MergeCommitPolicy::FirstParentOnly,
                    &cancel,
                )?;
                let terminal = oids.last().map(|o| o.to_string());
                let mut units: Vec<PrUnit> = Vec::new();
                for oid in oids {
                    let Ok(changes) = GitWalker::files_changed_in_commit(&repo, oid) else {
                        continue;
                    };
                    // Empty merges carry no exemplar value; >150 files is a
                    // bulk/vendoring commit, shape noise for imitation.
                    if changes.is_empty() || changes.len() > 150 {
                        continue;
                    }
                    let files: Vec<String> = changes
                        .iter()
                        .map(|c| c.path().as_str().replace('\\', "/"))
                        .collect();
                    let Ok(commit) = repo.find_commit(oid) else {
                        continue;
                    };
                    let summary = commit.summary().unwrap_or("").to_string();
                    let message = commit.message().unwrap_or("").to_string();
                    let author = commit.author().name().unwrap_or("unknown").to_string();
                    let timestamp = commit.time().seconds().max(0) as u64;
                    // Leak-free cutoff: skip anything merged on/after the
                    // snapshot date (strictly-before semantics).
                    if let Some(cutoff) = &cutoff
                        && crate::utils::ymd_utc(timestamp * 1000).as_str() >= cutoff.as_str()
                    {
                        continue;
                    }
                    let short: String = oid.to_string().chars().take(10).collect();
                    let (pr_id, title) = parse_pr_identity(&summary, &short);
                    // Body = message minus the summary line.
                    let body = message
                        .strip_prefix(&summary)
                        .unwrap_or(&message)
                        .trim()
                        .to_string();
                    units.push((pr_id, title, author, timestamp, body, files));
                }
                Ok((units, terminal, root_note))
            })
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| {
                McpError::internal_error(
                    format!("ingest_merged_prs: cannot walk git history: {e}"),
                    None,
                )
            })?;

        let mut docs: Vec<engram_index::IndexDoc> = Vec::with_capacity(units.len());
        let mut pr_count = 0usize;
        let mut direct_count = 0usize;
        for (pr_id, title, author, timestamp, body, files) in &units {
            if pr_id.starts_with("PR-") {
                pr_count += 1;
            } else {
                direct_count += 1;
            }
            let domains = classify_domains(files);
            let content = render_pr_doc(pr_id, title, author, *timestamp, body, &domains, files);
            // Path-stable identity (like business_logic): re-ingest upserts.
            let synthetic_path = format!("pr:{pr_id}");
            let path_hash = ContentHash::compute(synthetic_path.as_bytes());
            let doc_id = DocIdStr::compute(&synthetic_path, 0, 0, &path_hash);
            let content_hash = ContentHash::compute(content.as_bytes());
            docs.push(engram_index::IndexDoc {
                // Generation 0 (the GlobalMutable pattern): pr:<id> paths are
                // stable, so gen-0 pks give overwrite semantics AND survive
                // project reindexes. Ingesting at the live generation broke
                // every get_doc lookup (and the kind/date filters with it)
                // the moment the project was reindexed past that gen.
                generation: 0,
                chunk_id: {
                    let h = blake3::hash(synthetic_path.as_bytes());
                    let mut b = [0u8; 8];
                    b.copy_from_slice(&h.as_bytes()[..8]);
                    u64::from_le_bytes(b)
                },
                path: synthetic_path.into(),
                language: "markdown".into(),
                content,
                namespace: engram_core::namespaces::NAMESPACE_HISTORY.into(),
                author: Some(author.clone()),
                timestamp: Some(*timestamp),
                start_line: 0,
                end_line: 0,
                doc_id: doc_id.0,
                content_hash: content_hash.0,
            });
        }

        let indexed = docs.len();
        if !docs.is_empty() {
            ps.search
                .index_docs(
                    &req.project_id,
                    &docs,
                    &tokio_util::sync::CancellationToken::new(),
                )
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        }

        if req.rebuild {
            let current_paths: std::collections::BTreeSet<String> =
                docs.iter().map(|doc| doc.path.to_string()).collect();
            let obsolete: Vec<engram_core::RelPath> = previous_paths
                .difference(&current_paths)
                .map(|path| engram_core::RelPath::new(path))
                .collect();
            if !obsolete.is_empty() {
                ps.search
                    .delete_files(
                        &req.project_id,
                        engram_core::namespaces::NAMESPACE_HISTORY,
                        &obsolete,
                    )
                    .await
                    .map_err(|error| McpError::internal_error(error.to_string(), None))?;
            }
        }

        // A cutoff ingest must NOT advance the watermark: the walk visited
        // post-cutoff commits but skipped their units, and marking them
        // ingested would silently exclude them from any later full ingest.
        if merged_before.is_none()
            && let Some(t) = terminal
        {
            let reg = self.state.registry.clone();
            let pid = req.project_id.clone();
            let _ =
                tokio::task::spawn_blocking(move || reg.set_meta(&pid, watermark_key, &t)).await;
        }

        let mut out = format!(
            "# Merged-work corpus updated\n\
             change units indexed: {indexed} ({pr_count} merged PRs, {direct_count} direct commits)\n\
             walk root: {root_note}\n\
             namespace: history (docs `pr:<id>`)\n\
             next: find_merged_work(story=\"...\") to see how similar indexed work was done.\n"
        );
        if indexed == 0 {
            out.push_str(
                "(nothing new since the last ingest — the watermark only walks fresh commits)\n",
            );
        }
        out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_find_merged_work(
        &self,
        req: crate::models::FindMergedWorkRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;
        let top = req.top.clamp(1, 10);
        if req.file_paths.len() > 10 || req.file_paths.iter().any(|p| {
            p.trim().is_empty() || p.len() > 2048 || p.starts_with(['/', '\\'])
                || p.contains(':') || p.replace('\\', "/").split('/').any(|s| s == "..")
        }) {
            return Err(McpError::invalid_params("file_paths must contain at most 10 nonempty relative paths without traversal", None));
        }

        let merged_before = req
            .merged_before
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(str::to_string);
        if let Some(d) = &merged_before
            && (d.len() != 10
                || ymd_to_epoch_secs(d).is_none()
                || !d.chars().enumerate().all(|(i, c)| {
                    if i == 4 || i == 7 {
                        c == '-'
                    } else {
                        c.is_ascii_digit()
                    }
                }))
        {
            return Err(McpError::invalid_params(
                format!("merged_before must be YYYY-MM-DD, got '{d}'"),
                None,
            ));
        }

        let kind_filter = req
            .kind
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .map(str::to_lowercase);
        let q = engram_index::HybridQuery {
            project_id: req.project_id.clone(),
            namespace: engram_core::namespaces::NAMESPACE_HISTORY.into(),
            generation: gen_,
            text: req.story.clone(),
            // Rank a bounded pool of eligible documents, not just the final
            // top-N BM25 hits (repeated boilerplate can monopolize those).
            top_k: 100,
            fts_mode: "loose".into(),
            // Only the PR-level docs — not raw commit messages or diffs.
            include_path_prefixes: Some(vec!["pr:".into()]),
            exclude_path_prefixes: None,
            include_path_suffixes: None,
            language_filters: None,
            author_filter: None,
            date_after: None,
            // Cutoff INSIDE the query (strictly before the date): post-cutoff
            // docs must not eat top_k slots, or the survivors shift whenever
            // the corpus gains newer PRs. The display-time string check below
            // stays as belt-and-braces.
            date_before: merged_before
                .as_deref()
                .and_then(ymd_to_epoch_secs)
                .map(|s| s.saturating_sub(1)),
            use_mmr: false,
        };
        let engine = ps.search.clone();
        let selected_kind = kind_filter.clone();
        let cutoff = merged_before.clone();
        let story = req.story.clone();
        let file_paths = req.file_paths.clone();
        let (hits, title_ranks, candidate_count, candidate_lists_capped) =
            tokio::task::spawn_blocking(move || {
                let mut ranks = HashMap::new();
                let mut queries = vec![q.clone()];
                let primary = primary_task_text(&story);
                if primary != story {
                    let mut task_query = q.clone();
                    task_query.text = primary.to_string();
                    queries.push(task_query);
                }
                // File history remains discoverable when a new task uses words
                // absent from an older regression title. Eligibility still
                // requires a shipped-cohort match, not a prose mention.
                for path in &file_paths {
                    let mut file_query = q.clone();
                    file_query.text = path.replace('\\', "/").rsplit('/').next().unwrap_or(path).to_string();
                    if !queries.iter().any(|query| query.text == file_query.text) {
                        queries.push(file_query);
                    }
                }
                let mut candidates = HashMap::new();
                let mut support: HashMap<String, f32> = HashMap::new();
                let mut capped = false;
                for query in queries {
                    let mut seen = std::collections::HashSet::new();
                    let found =
                        engine.lexical_search_matching(&query, &mut |doc_id, content| {
                            let eligible = merged_document_matches(
                                content,
                                selected_kind.as_deref(),
                                cutoff.as_deref(),
                            ) && cohort_path_matches(content, &file_paths)
                                && seen.insert(doc_id.to_string());
                            if eligible {
                                ranks.insert(
                                    doc_id.to_string(),
                                    exemplar_title_rank(&story, content),
                                );
                            }
                            eligible
                        })?;
                    capped |= found.len() == 100;
                    for (position, hit) in found.into_iter().enumerate() {
                        // Rank fusion avoids comparing raw BM25 scores from
                        // different query lengths. It is support, not probability.
                        *support.entry(hit.doc_id.clone()).or_default() +=
                            1.0 / (61.0 + position as f32);
                        candidates.entry(hit.doc_id.clone()).or_insert(hit);
                    }
                }
                let mut hits: Vec<_> = candidates.into_values().collect();
                for hit in &mut hits {
                    hit.score = support[&hit.doc_id];
                }
                let candidate_count = hits.len();
                hits.sort_by(|a, b| {
                    ranks
                        .get(&b.doc_id)
                        .cmp(&ranks.get(&a.doc_id))
                        .then_with(|| b.score.total_cmp(&a.score))
                        .then_with(|| a.doc_id.cmp(&b.doc_id))
                });
                hits.truncate(top);
                Ok::<_, anyhow::Error>((hits, ranks, candidate_count, capped))
            })
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if hits.is_empty() {
            let mut out = "result: no merged work matched.\n\
                 hints: run ingest_merged_prs first (one-time per repo, then \
                 incremental); or broaden the story wording — titles use the \
                 team's vocabulary, try domain terms from get_concept_footprint."
                .to_string();
            out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
            return Ok(CallToolResult::success(vec![Content::text(out)]));
        }

        let mut out = format!(
            "# How similar indexed work was done — '{}'{}\n",
            req.story,
            kind_filter
                .as_deref()
                .map(|k| format!(" [kind: {k}]"))
                .unwrap_or_default()
        );
        out.push_str("Evidence: indexed git history; review approvals were not fetched. Direct commits may also appear.\n");
        if !req.file_paths.is_empty() {
            out.push_str("File scope: shipped cohort paths, exact or multi-segment suffix matches. Suffix matches are leads only; root/rename equivalence and applicability to this method are not proven.\n");
        }
        out.push_str(&format!("Ranking: primary-task title matches, then full-query title matches and fused lexical rank; {candidate_count} eligible candidates inspected (limit 100 per query, at most {}{}).\n", 200 + 100 * req.file_paths.len(),
            if candidate_lists_capped { "; a candidate list reached its limit, so broader candidates were not reranked" } else { "" }));
        let mut shown = 0usize;
        // A rebuilt corpus can briefly hold the same PR at two generations
        // (gen-0 + a legacy gen) — same doc_id, two pks. Render each once.
        let mut seen_doc_ids: std::collections::HashSet<&str> = Default::default();
        for h in &hits {
            if shown >= top {
                break;
            }
            if !seen_doc_ids.insert(h.doc_id.as_str()) {
                continue;
            }
            // PR docs live at generation 0 (stable pks; see ingest). Fall
            // back to the live generation for corpora ingested before that
            // change so old installs keep working until a rebuild.
            let fetched = match ps.search.get_doc_by_doc_id(
                &req.project_id,
                engram_core::namespaces::NAMESPACE_HISTORY,
                0,
                &h.doc_id,
            ) {
                Ok(Some(d)) => Ok(Some(d)),
                _ => ps.search.get_doc_by_doc_id(
                    &req.project_id,
                    engram_core::namespaces::NAMESPACE_HISTORY,
                    gen_,
                    &h.doc_id,
                ),
            };
            match fetched {
                Ok(Some((_, _, content, _, _))) => {
                    if !merged_document_matches(
                        &content,
                        kind_filter.as_deref(),
                        merged_before.as_deref(),
                    ) || !cohort_path_matches(&content, &req.file_paths) {
                        continue;
                    }
                    shown += 1;
                    let (primary_phrases, primary_terms, phrases, terms) =
                        title_ranks.get(&h.doc_id).copied().unwrap_or_default();
                    out.push_str(&format!("\n## match #{shown} (primary title phrases: {primary_phrases}, primary title terms: {primary_terms}, full title phrases: {phrases}, full title terms: {terms}, fused lexical rank {:.4})\n", h.score));
                    let view = exemplar_view(&content, 60).replace(
                        "## Files shipped together in this approved change",
                        "## Files shipped together in this change",
                    );
                    out.push_str(&view);
                    // PR identity comes from the stored document, not the search
                    // query. Imported decisions never silently suppress results.
                    if merged_before.is_some() {
                        out.push_str("Review decisions omitted for point-in-time replay: imported event times are not independently verified.\n");
                    } else if let Some(review_id) = content
                        .lines()
                        .next()
                        .and_then(|line| line.strip_prefix("# "))
                        .and_then(|line| line.split_whitespace().next())
                        .map(|s| s.trim_end_matches(':'))
                    {
                        if review_id.starts_with("PR-") {
                            let rec = self.ensure_project_record(&req.project_id).await?;
                            match super::review_decisions::snapshot(
                                &self.state,
                                &req.project_id,
                                review_id,
                                std::path::Path::new(&rec.directory),
                            ) {
                                Ok(mut decisions) => {
                                    decisions.as_object_mut().map(|v| v.remove("events"));
                                    if let Some(current) = decisions["current"].as_array_mut() {
                                        let total = current.len();
                                        current.truncate(20);
                                        decisions["total_current_decisions"] = total.into();
                                    }
                                    out.push_str(&format!("\nReview decision evidence (at most 20; use get_review_decisions for full history):\n{}\n", decisions));
                                }
                                Err(error) => out.push_str(&format!(
                                    "\nReview decisions unavailable: {error}\n"
                                )),
                            }
                        }
                    }
                    let recovery = serde_json::json!({
                        "project_id": req.project_id,
                        "doc_id": h.doc_id,
                        "namespace": "history",
                    });
                    out.push_str(&format!("full_document: get_chunk({recovery})\n"));
                    if content.lines().any(|line| line.starts_with("... and ")) {
                        out.push_str("LEGACY_TRUNCATED: rebuild the merged corpus to recover the complete stored file cohort.\n");
                    }
                }
                _ => {
                    if req.file_paths.is_empty() && kind_filter.is_none()
                        && merged_before.is_none()
                        && let Some(sn) = &h.snippet
                    {
                        shown += 1;
                        out.push_str(&format!("\n## match #{shown} (score {:.3})\n", h.score));
                        out.push_str(
                            "[INCOMPLETE: full PR document unavailable; search excerpt only]\n",
                        );
                        out.push_str(sn);
                        out.push('\n');
                    }
                }
            }
        }
        if shown == 0 {
            out.push_str(
                "\n(no matches passed the kind filter — kinds: ui-markup, ui-code, js, \
                 database, settings, resources, api, backend; drop the filter to see all)\n",
            );
        }
        out.push_str(
            "\nnext: inspect the closest change and its consumers for applicability; a shared file is not proof that its solution should be copied. get_change_set(story=...) \
             combines history with concept/graph evidence into a ranked file list.\n",
        );
        out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }
}

fn cohort_path_matches(content: &str, paths: &[String]) -> bool {
    if paths.is_empty() { return true; }
    let Some((_, cohort)) = content.rsplit_once("## Files shipped together") else { return false; };
    cohort.lines().skip(1).take_while(|line| !line.starts_with("## "))
        .filter_map(|line| line.strip_prefix("- "))
        .any(|shipped| paths.iter().any(|path| {
            let shipped = shipped.trim().replace('\\', "/");
            let path = path.trim().replace('\\', "/");
            shipped == path || (shipped.contains('/') && path.ends_with(&format!("/{shipped}")))
                || (path.contains('/') && shipped.ends_with(&format!("/{path}")))
        }))
}

fn merged_document_matches(content: &str, kind: Option<&str>, cutoff: Option<&str>) -> bool {
    let kind_matches = kind.is_none_or(|kind| {
        content
            .lines()
            .find_map(|line| line.split("| kinds: ").nth(1))
            .is_some_and(|kinds| {
                kinds
                    .split(',')
                    .any(|value| value.trim().eq_ignore_ascii_case(kind))
            })
    });
    let before_cutoff = cutoff.is_none_or(|cut| {
        content
            .lines()
            .find_map(|line| line.strip_prefix("merged: "))
            .and_then(|rest| rest.get(..10))
            .is_some_and(|date| ymd_to_epoch_secs(date).is_some() && date < cut)
    });
    kind_matches && before_cutoff
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_rank_normalizes_identifiers_and_plurals_without_joining_stopwords() {
        assert_eq!(
            exemplar_title_rank(
                "how create invoices from selected orders",
                "# PR-1: CreateInvoices from SelectedOrders"
            ),
            (1, 2, 2, 4)
        );
        assert_eq!(
            exemplar_title_rank("invoice from orders", "# PR-1: Invoice orders"),
            (0, 1, 0, 2)
        );
        assert_eq!(
            exemplar_title_rank("this invoice", "# PR-1: This unrelated update"),
            (0, 0, 0, 0)
        );
        assert_eq!(
            exemplar_title_rank("change request", "# PR-1: +[Change] Invoice rows"),
            (0, 0, 0, 0)
        );
        assert_eq!(
            exemplar_title_rank("billing", "# PR-1: [Billing] Invoice rows"),
            (0, 1, 0, 1)
        );
    }

    #[test]
    fn identifiers_and_quoted_context_words_do_not_split_the_task() {
        let rank = exemplar_title_rank("LoadFromStorage", "# PR-1: LoadFromStorage");
        assert_eq!((rank.0, rank.1), (rank.2, rank.3));
        let rank = exemplar_title_rank("\"load from storage\"", "# PR-1: load from storage");
        assert_eq!((rank.0, rank.1), (rank.2, rank.3));
    }

    #[test]
    fn merged_metadata_filters_require_exact_kinds_and_valid_prior_dates() {
        let doc = "merged: 2026-01-03 | author: author | kinds: ui-code, database\n";
        assert!(merged_document_matches(
            doc,
            Some("database"),
            Some("2026-01-04")
        ));
        assert!(!merged_document_matches(doc, Some("data"), None));
        assert!(!merged_document_matches(doc, None, Some("2026-01-03")));
        assert!(!merged_document_matches(
            "merged: 2026-02-31 | kinds: database",
            None,
            Some("2027-01-01")
        ));
        assert!(!merged_document_matches(
            "no metadata",
            None,
            Some("2027-01-01")
        ));
    }

    #[test]
    fn parse_ado_and_github_pr_identities() {
        let (id, title) = parse_pr_identity("Merged PR 1955: Fix role gating on user edit", "ab12");
        assert_eq!(id, "PR-1955");
        assert_eq!(title, "Fix role gating on user edit");

        let (id, title) = parse_pr_identity(
            "Merge pull request #42 from org/feature-x Add export",
            "ab12",
        );
        assert_eq!(id, "PR-42");
        assert!(title.contains("Add export"), "{title}");

        let (id, title) = parse_pr_identity("plain direct commit", "ab12cd34ef");
        assert_eq!(id, "commit-ab12cd34ef");
        assert_eq!(title, "plain direct commit");
    }

    #[test]
    fn classify_domains_picks_dominant_dirs_and_skips_vendor() {
        let files = vec![
            "Site/modules/dashboard/pages/admin/system/user/user_edit.aspx".to_string(),
            "Site/modules/dashboard/pages/admin/system/user/user_edit.aspx.vb".to_string(),
            "Site/modules/dashboard/pages/admin/system/user/user.aspx.vb".to_string(),
            "Site/App_Code/users-security/code/aspnetUsers.vb".to_string(),
            "Site/bower_components/x/y.min.js".to_string(),
        ];
        let domains = classify_domains(&files);
        assert_eq!(domains.len(), 2, "{domains:?}");
        assert!(
            domains[0].contains("site/modules/dashboard/pages"),
            "{domains:?}"
        );
        assert!(
            !domains.iter().any(|d| d.contains("bower_components")),
            "vendor dirs must not become domains: {domains:?}"
        );
    }

    #[test]
    fn pr_document_preserves_full_cohort_and_view_is_compact() {
        let files: Vec<String> = (0..70).map(|i| format!("dir/file{i}.vb")).collect();
        let doc = render_pr_doc(
            "PR-9",
            "Add department field",
            "dev",
            1_750_000_000,
            "body text",
            &["dir".into()],
            &files,
        );
        assert!(doc.contains("# PR-9: Add department field"));
        assert!(doc.contains("files: 70"));
        assert!(doc.contains("dir/file69.vb"), "{doc}");
        assert!(!doc.contains("... and "), "{doc}");
        let view = exemplar_view(&doc, 60);
        assert!(view.contains("... and 10 more"), "{view}");
        assert!(!view.contains("dir/file69.vb"), "{view}");
    }

    #[test]
    fn exemplar_view_reaches_cohort_past_long_body() {
        // A body near the 600-char render cap: the old 500-char head cut
        // ended inside it and the cohort never appeared in the dossier.
        let body = "word ".repeat(115); // ~575 chars
        let files: Vec<String> = (0..30).map(|i| format!("dir/file{i}.vb")).collect();
        let doc = render_pr_doc(
            "PR-9",
            "Add department field",
            "dev",
            1_750_000_000,
            &body,
            &["dir".into()],
            &files,
        );
        let view = exemplar_view(&doc, 20);
        assert!(view.contains("# PR-9: Add department field"));
        assert!(view.contains("merged: "));
        assert!(
            view.contains("## Files shipped together"),
            "cohort header must survive: {view}"
        );
        assert!(view.contains("- dir/file0.vb"));
        assert!(view.contains("- dir/file19.vb"));
        // 30 files, 20 shown → 10 folded.
        assert!(!view.contains("- dir/file20.vb"));
        assert!(view.contains("... and 10 more"), "{view}");
        // Body capped at two lines: the repeated filler is one long line,
        // so it appears once, not verbatim-in-full beyond that.
        assert!(view.len() < doc.len());
    }

    #[test]
    fn exemplar_view_folds_doc_overflow_marker() {
        // Storage retains the full cohort; only the displayed view is capped.
        let files: Vec<String> = (0..70).map(|i| format!("dir/file{i}.vb")).collect();
        let doc = render_pr_doc(
            "PR-9",
            "t",
            "dev",
            1_750_000_000,
            "",
            &["dir".into()],
            &files,
        );
        let view = exemplar_view(&doc, 20);
        assert!(doc.contains("dir/file69.vb"));
        assert!(!doc.contains("... and "));
        assert!(!view.contains("dir/file69.vb"));
        assert!(view.contains("... and 50 more"), "{view}");
    }

    #[tokio::test]
    async fn failed_git_walk_during_rebuild_preserves_existing_corpus() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        let cfg = engram_core::Config {
            data_dir: tmp.path().join("data"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            ..Default::default()
        };
        let (state, _rx) = crate::AppState::new(cfg).unwrap();
        let pid = "rebuild-preservation";
        state
            .registry
            .put_project(&engram_core::ProjectRecord {
                project_id: pid.into(),
                project_name: pid.into(),
                directory: root.to_string_lossy().into_owned(),
                project_type: "general".into(),
                created_at_ms: 0,
                updated_at_ms: 0,
                reindex_required_since_ms: None,
            })
            .unwrap();
        state
            .registry
            .set_meta(pid, "active_generation", "1")
            .unwrap();
        let engram = Engram::new(state);
        let runtime = engram.ensure_project_runtime(pid).await.unwrap();
        let content = "preserved historical evidence";
        let doc = engram_index::IndexDoc {
            generation: 0,
            chunk_id: 1,
            path: engram_core::RelPath::new("pr:PR-1"),
            language: "markdown".into(),
            content: content.into(),
            namespace: "history".into(),
            author: None,
            timestamp: None,
            start_line: 0,
            end_line: 0,
            doc_id: "preserved".into(),
            content_hash: ContentHash::compute(content.as_bytes()).0,
        };
        runtime
            .search
            .index_docs(pid, &[doc], &tokio_util::sync::CancellationToken::new())
            .await
            .unwrap();
        let result = engram
            .handle_ingest_merged_prs(crate::models::IngestMergedPrsRequest {
                project_id: pid.into(),
                rebuild: true,
                max_commits: 5,
                merged_before: None,
            })
            .await;
        assert!(
            result.is_err(),
            "fixture deliberately has no git repository"
        );
        assert!(
            runtime
                .search
                .get_doc_by_doc_id(pid, "history", 0, "preserved")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn layer_profile_classifies_kinds_lines() {
        use super::layer_profile;
        assert_eq!(
            layer_profile("api, backend, ui-code, ui-markup"),
            (true, true)
        );
        assert_eq!(layer_profile("js"), (true, false));
        assert_eq!(layer_profile("backend, database"), (false, true));
        // Layer-neutral kinds alone -> neither side.
        assert_eq!(layer_profile("settings, resources"), (false, false));
        assert_eq!(layer_profile("-"), (false, false));
    }

    #[test]
    fn ymd_to_epoch_round_trips_with_ymd_utc() {
        use super::ymd_to_epoch_secs;
        assert_eq!(ymd_to_epoch_secs("1970-01-01"), Some(0));
        for d in ["2000-02-29", "2026-05-14", "2026-12-31", "2024-03-01"] {
            let secs = ymd_to_epoch_secs(d).expect(d);
            assert_eq!(crate::utils::ymd_utc(secs * 1000), d, "round-trip {d}");
        }
        assert!(ymd_to_epoch_secs("garbage").is_none());
        assert!(ymd_to_epoch_secs("2026-13-01").is_none());
        assert!(ymd_to_epoch_secs("2026-00-10").is_none());
    }

    #[test]
    fn exemplar_view_skips_template_heading_lines_in_body() {
        // ADO PR descriptions carry template headings ("###Task/work
        // completed") — labels, not prose. The two body slots must go to
        // content lines (live sighting: PR-1968 exemplar).
        let body =
            "###Task/work completed\nFixes tenant filtering.\n### How to test\nAssign a resource.";
        let doc = render_pr_doc(
            "PR-9",
            "t",
            "dev",
            1_750_000_000,
            body,
            &[],
            &["a.vb".into()],
        );
        let view = exemplar_view(&doc, 20);
        assert!(!view.contains("###Task/work completed"), "{view}");
        assert!(!view.contains("### How to test"), "{view}");
        assert!(view.contains("Fixes tenant filtering."));
        assert!(view.contains("Assign a resource."));
        assert!(view.contains("- a.vb"));
    }
}
