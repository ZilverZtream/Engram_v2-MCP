//! Merged-work corpus: PR-level exemplars of APPROVED changes.
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
//!   showing the approved file cohort to mirror.

use crate::handlers::validate_project_id;
use crate::tools::Engram;
use engram_core::{ContentHash, DocIdStr};
use engram_git::{GitWalker, history::MergeCommitPolicy};
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use std::collections::HashMap;
use std::sync::LazyLock;

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
    let mut add = |k: &'static str, kinds: &mut Vec<&'static str>| {
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
    let trimmed_body: String = body.trim().chars().take(600).collect();
    if !trimmed_body.is_empty() && trimmed_body != title {
        md.push_str(&format!("\n{trimmed_body}\n"));
    }
    md.push_str("\n## Files shipped together in this approved change\n");
    for f in files.iter().take(60) {
        md.push_str(&format!("- {f}\n"));
    }
    if files.len() > 60 {
        md.push_str(&format!("... and {} more\n", files.len() - 60));
    }
    md
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

        if req.rebuild {
            // Stable-pk upserts only overwrite ids that recur; a rebuild
            // must clear the previous pr:* docs first, or stale ids from a
            // differently-rooted walk linger and can outrank fresh docs
            // (observed live: an unmerged branch commit stayed match #1
            // after the approved-root fix). Scoped to pr:* paths — the
            // history namespace also carries revert/insight docs owned by
            // index_git_history.
            let stale: std::collections::BTreeSet<String> = ps
                .search
                .list_docs_for_project(&req.project_id)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
                .into_iter()
                .filter(|d| {
                    d.namespace == engram_core::namespaces::NAMESPACE_HISTORY
                        && d.path.starts_with("pr:")
                })
                .map(|d| d.path)
                .collect();
            if !stale.is_empty() {
                let paths: Vec<engram_core::RelPath> =
                    stale.iter().map(|p| engram_core::RelPath::new(p)).collect();
                ps.search
                    .delete_files(
                        &req.project_id,
                        engram_core::namespaces::NAMESPACE_HISTORY,
                        &paths,
                    )
                    .await
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            }
        }

        let repo_dir = std::path::PathBuf::from(&rec.directory);
        type PrUnit = (String, String, String, u64, String, Vec<String>);
        let (units, terminal, root_note): (Vec<PrUnit>, Option<String>, &'static str) =
            tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                let repo = GitWalker::open_repo(&repo_dir)?;
                let cancel = tokio_util::sync::CancellationToken::new();
                // Walk the REMOTE DEFAULT branch, not the checkout: this
                // corpus presents itself as merged/APPROVED work, and a
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

        if let Some(t) = terminal {
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
             next: find_merged_work(story=\"...\") to see how similar approved work was done.\n"
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

        let q = engram_index::HybridQuery {
            project_id: req.project_id.clone(),
            namespace: engram_core::namespaces::NAMESPACE_HISTORY.into(),
            generation: gen_,
            text: req.story.clone(),
            top_k: top * 3,
            fts_mode: "loose".into(),
            // Only the PR-level docs — not raw commit messages or diffs.
            include_path_prefixes: Some(vec!["pr:".into()]),
            exclude_path_prefixes: None,
            language_filters: None,
            author_filter: None,
            date_after: None,
            date_before: None,
            use_mmr: false,
        };
        let engine = ps.search.clone();
        let hits = tokio::task::spawn_blocking(move || engine.lexical_search(&q))
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

        let kind_filter = req
            .kind
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .map(str::to_lowercase);
        let merged_before = req
            .merged_before
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(str::to_string);
        if let Some(d) = &merged_before
            && (d.len() != 10
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
        let mut out = format!(
            "# How similar approved work was done — '{}'{}\n",
            req.story,
            kind_filter
                .as_deref()
                .map(|k| format!(" [kind: {k}]"))
                .unwrap_or_default()
        );
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
                    // Ultra-coarse kind filter: match against the doc's
                    // `kinds:` line so "database" only returns exemplars
                    // that actually shipped SQL.
                    if let Some(k) = &kind_filter {
                        let has_kind = content
                            .lines()
                            .find(|l| l.contains("| kinds: "))
                            .is_some_and(|l| l.to_lowercase().contains(k.as_str()));
                        if !has_kind {
                            continue;
                        }
                    }
                    // Point-in-time replay / leak-free eval: drop exemplars
                    // merged ON or AFTER the cutoff. ISO dates compare
                    // correctly as strings against the doc's `merged:` line.
                    if let Some(cut) = &merged_before {
                        let leaks = content
                            .lines()
                            .find_map(|l| l.split("merged: ").nth(1))
                            .and_then(|rest| rest.get(..10))
                            .is_none_or(|d| d >= cut.as_str());
                        if leaks {
                            continue;
                        }
                    }
                    shown += 1;
                    out.push_str(&format!("\n## match #{shown} (score {:.3})\n", h.score));
                    out.push_str(&content);
                }
                _ => {
                    if kind_filter.is_none()
                        && let Some(sn) = &h.snippet
                    {
                        shown += 1;
                        out.push_str(&format!("\n## match #{shown} (score {:.3})\n", h.score));
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
            "\nnext: mirror the file cohort of the closest match; get_change_set(story=...) \
             fuses these exemplars with concept/graph evidence into a ranked file list.\n",
        );
        out.push_str(&self.freshness_footer(&req.project_id, gen_).await);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn pr_doc_is_compact_and_carries_cohort() {
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
        assert!(doc.contains("... and 10 more"), "{doc}");
    }
}
