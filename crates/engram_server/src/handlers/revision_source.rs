use crate::{models::*, tools::Engram};
use anyhow::{Context, Result, bail, ensure};
use engram_index::grep::{self, FreshnessMode, GrepQuery};
use git2::{Oid, Repository, TreeWalkMode, TreeWalkResult};
use rmcp::{
    ErrorData as McpError,
    model::{CallToolResult, Content},
};
use serde_json::{Value, json};
use std::path::{Component, Path};

/// HEAD describes the observed checkout. Dirty source and an older published
/// index can both differ from it, so give the identity a distinct name.
pub(crate) async fn checkout_basis(server: &Engram, project: &str) -> Option<Value> {
    let rec = server.ensure_project_record(project).await.ok()?;
    let root = server.state.paths.resolve_path(&rec.directory).ok()?;
    tokio::task::spawn_blocking(move || {
        let repo = Repository::discover(root).ok()?;
        let head = repo.head().ok()?;
        let oid = head.peel_to_commit().ok()?.id();
        Some(
            json!({"kind":"working_checkout","observed_head":oid.to_string(),
            "branch":head.shorthand().filter(|_|head.is_branch()),
            "qualification":"Working files and indexed content can differ from HEAD."}),
        )
    })
    .await
    .ok()
    .flatten()
}

pub(crate) fn label_checkout(result: &mut CallToolResult, basis: Value) {
    let Some(content) = result.content.first_mut() else {
        return;
    };
    let rmcp::model::RawContent::Text(text) = &mut content.raw else {
        return;
    };
    if let Ok(Value::Object(mut report)) = serde_json::from_str(&text.text) {
        report.insert("checkout_identity".into(), basis);
        text.text = Value::Object(report).to_string();
    } else {
        text.text = format!(
            "Observed checkout {} at {} (indexed and working bytes can differ)\n{}",
            basis["branch"].as_str().unwrap_or("(detached)"),
            basis["observed_head"].as_str().unwrap_or("(unavailable)"),
            text.text
        );
    }
}

pub(crate) enum Query {
    Grep(GrepProjectRequest),
    Body(GetFullMethodBodyRequest),
    References(FindSymbolReferencesRequest),
    Pattern(FindImplementationPatternRequest),
    Footprint(GetConceptFootprintRequest),
}

impl Query {
    fn identity(&self) -> (&str, Option<&str>, Option<usize>) {
        match self {
            Self::Grep(r) => (&r.project_id, r.as_of_rev.as_deref(), r.max_tokens),
            Self::Body(r) => (&r.project_id, r.as_of_rev.as_deref(), r.max_tokens),
            Self::References(r) => (&r.project_id, r.as_of_rev.as_deref(), None),
            Self::Pattern(r) => (&r.project_id, r.as_of_rev.as_deref(), r.max_tokens),
            Self::Footprint(r) => (&r.project_id, r.as_of_rev.as_deref(), r.max_tokens),
        }
    }
}

pub(crate) async fn read(server: &Engram, query: Query) -> Result<CallToolResult, McpError> {
    let (project, _, budget) = query.identity();
    let rec = server.ensure_project_record(project).await?;
    let root = server
        .state
        .paths
        .resolve_path(&rec.directory)
        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
    let limit = super::search_tools::response_budget::token_bytes(budget)?.unwrap_or(40 * 1024);
    let output = tokio::task::spawn_blocking(move || gather(&root, &query, limit))
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::invalid_params(format!("{e:#}"), None))?;
    Ok(CallToolResult::success(vec![Content::text(output)]))
}

const FILE_LIMIT: usize = 4 * 1024 * 1024;

fn gather(root: &Path, query: &Query, limit: usize) -> Result<String> {
    let (_, revision, _) = query.identity();
    let revision = revision.context("missing revision")?.trim();
    ensure!(
        !revision.is_empty() && revision.len() <= 256 && !revision.chars().any(char::is_control),
        "invalid revision"
    );
    let repo = Repository::discover(root)?;
    let (object, reference) = repo
        .revparse_ext(revision)
        .context("requested Git revision is unavailable")?;
    let commit = object.peel_to_commit()?;
    let project = root.canonicalize()?;
    let workdir = repo
        .workdir()
        .context("project requires a Git worktree")?
        .canonicalize()?;
    let prefix = project.strip_prefix(&workdir)?;
    let root_tree = commit.tree()?;
    let tree = if prefix.as_os_str().is_empty() {
        root_tree
    } else {
        repo.find_tree(root_tree.get_path(prefix)?.id())?
    };
    let mut files = Vec::new();
    let mut partial = false;
    if let Query::Body(req) = query
        && let Some(path) = &req.file_path
    {
        let path = path.replace('\\', "/");
        ensure!(
            !path.is_empty()
                && Path::new(&path)
                    .components()
                    .all(|c| matches!(c, Component::Normal(_))),
            "expected project-relative path"
        );
        let entry = tree
            .get_path(Path::new(&path))
            .context("file absent at requested revision")?;
        ensure!(
            matches!(entry.filemode(), 0o100644 | 0o100755),
            "expected a regular file at the requested revision"
        );
        files.push((path, entry.id()));
    } else {
        tree.walk(TreeWalkMode::PreOrder, |dir, entry| {
            if files.len() >= 100_000 {
                partial = true;
                return TreeWalkResult::Abort;
            }
            if matches!(entry.filemode(), 0o100644 | 0o100755)
                && let Some(name) = entry.name()
            {
                files.push((format!("{dir}{name}"), entry.id()));
            }
            TreeWalkResult::Ok
        })?;
    }
    let rows = match query {
        Query::Grep(r) => {
            ensure!(
                r.namespace == "memory" && !r.pattern.is_empty(),
                "pinned grep requires a source pattern in memory"
            );
            if r.regex {
                grep::compile_regex(&r.pattern, r.case_sensitive, r.multiline)?;
            }
            let cap = r.max_results.clamp(1, 1000);
            let q = GrepQuery {
                project_id: r.project_id.clone(),
                namespace: r.namespace.clone(),
                generation: 0,
                pattern: r.pattern.clone(),
                regex: r.regex,
                case_sensitive: r.case_sensitive,
                multiline: r.multiline,
                path_prefix: r.path_prefix.clone(),
                exclude_path_prefixes: r.exclude_path_prefixes.clone(),
                language: r.language.clone(),
                context_before: r.context_before.min(100),
                context_after: r.context_after.min(100),
                max_results: cap,
                freshness: FreshnessMode::Off,
            };
            let mut hits = Vec::new();
            scan(
                &repo,
                &files,
                &mut partial,
                |path| q.includes_source_path(path),
                |path, text| {
                    hits.extend(grep::scan_working_tree_content(text, path, &q)?);
                    Ok(hits.len() < cap)
                },
            )?;
            hits.truncate(cap);
            let hits: Vec<_> = hits.into_iter().skip(r.offset).collect();
            json!({"matches":hits,"tier_used":"git_tree","offset":r.offset})
        }
        Query::Body(r) => {
            ensure!(
                r.response_id.is_none() && r.response_offset == 0 && !r.include_caller_bodies,
                "pinned source cannot expand a current graph or recover a saved checkout report"
            );
            ensure!(
                r.file_path.is_some() != r.fqn.is_some(),
                "select exactly one of file_path and fqn"
            );
            ensure!(
                r.source_ranges.is_none()
                    || (r.file_path.is_some()
                        && r.line_start.is_none()
                        && r.line_end.is_none()
                        && r.context_lines == 0),
                "invalid combination for source_ranges"
            );
            ensure!(
                r.fqn.is_none()
                    || (r.line_start.is_none()
                        && r.line_end.is_none()
                        && r.expected_file_blake3.is_none()),
                "file options require file_path"
            );
            let mut results = Vec::new();
            scan(
                &repo,
                &files,
                &mut partial,
                |_| true,
                |path, text| {
                    let lines: Vec<_> = text.lines().collect();
                    let ranges = if let Some(fqn) = &r.fqn {
                        let short = fqn.rsplit(['.', ':']).next().unwrap_or(fqn);
                        if !text.to_lowercase().contains(&short.to_lowercase()) {
                            return Ok(true);
                        }
                        let syms = if path.to_lowercase().ends_with(".vb") {
                            engram_index::vb_extractor::snapshot_declaration_candidates(text)
                                .map_err(anyhow::Error::msg)?
                        } else {
                            engram_index::parsing::SymbolExtractor::new()
                                .extract(Path::new(path), text)
                                .0
                        };
                        syms.into_iter()
                            .filter(|s| {
                                s.kind == "function"
                                    && (s.name == *fqn
                                        || (path.to_lowercase().ends_with(".vb")
                                            && s.name.eq_ignore_ascii_case(fqn)))
                            })
                            .map(|s| (s.start_line, s.end_line))
                            .collect()
                    } else if let Some(ranges) = &r.source_ranges {
                        ensure!(
                            (1..=3).contains(&ranges.len()),
                            "source_ranges requires one to three ranges"
                        );
                        ranges.iter().map(|s| (s.line_start, s.line_end)).collect()
                    } else {
                        ensure!(
                            r.line_end.is_none() || r.line_start.is_some(),
                            "line_end requires line_start"
                        );
                        let first = r.line_start.unwrap_or(1);
                        vec![(
                            first,
                            r.line_end.unwrap_or_else(|| {
                                first.saturating_add(119).min(lines.len() as u32)
                            }),
                        )]
                    };
                    let digest = blake3::hash(text.as_bytes()).to_hex();
                    ensure!(
                        r.expected_file_blake3
                            .as_ref()
                            .is_none_or(|expected| expected == digest.as_str()),
                        "pinned file hash mismatch"
                    );
                    let mut last = 0;
                    let mut count = 0;
                    for (start, end) in ranges {
                        ensure!(
                            start > last && end >= start && end as usize <= lines.len(),
                            "range outside pinned source or overlapping ranges"
                        );
                        count += end - start + 1;
                        ensure!(
                            r.source_ranges.is_none() || count <= 120,
                            "source_ranges exceeds 120 lines"
                        );
                        results.push(json!({"file_path":path,"line_start":start,"line_end":end,"total_lines":lines.len(),"file_blake3":digest.as_str(),"source_code":lines[start as usize-1..end as usize].join("\n")}));
                        last = end;
                    }
                    Ok(true)
                },
            )?;
            ensure!(
                !results.is_empty(),
                "no matching source span at requested revision"
            );
            ensure!(
                r.fqn.is_none() || (results.len() == 1 && !partial),
                "pinned method selection ambiguous or incomplete; supply a path and line range"
            );
            let path = results[0]["file_path"]
                .as_str()
                .context("missing historical path")?
                .to_owned();
            let mut core = if r.source_ranges.is_some() {
                json!({"windows":results})
            } else {
                results.remove(0)
            };
            if r.file_path.is_some()
                && r.line_end.is_none()
                && r.source_ranges.is_none()
                && let (Some(end), Some(total)) =
                    (core["line_end"].as_u64(), core["total_lines"].as_u64())
                && end < total
            {
                core["file_excerpt"] = json!({"next_page":{"tool":"get_full_method_body","arguments":{
                    "project_id":r.project_id,"file_path":path,"line_start":end+1,"context_lines":0,"output_json":r.output_json,
                    "expected_file_blake3":core["file_blake3"],"as_of_rev":commit.id().to_string(),"max_tokens":r.max_tokens}}});
            }
            let policy = crate::services::project_policy::resolve_at_revision(
                root,
                commit.id(),
                &[path],
                &[],
            )?;
            core["project_policy_sources"] = policy.planning_index(&r.project_id);
            core
        }
        _ => source_sites(&repo, &files, query, &mut partial)?,
    };
    let branch = reference
        .as_ref()
        .filter(|r| r.is_branch() || r.is_remote())
        .and_then(|r| r.shorthand());
    let mut value = rows;
    value["source_basis"] = json!({"kind":"git_commit","requested_rev":revision,"branch":branch,"commit":commit.id().to_string()});
    value["scan_limited"] = json!(partial);
    if let Query::Grep(request) = query {
        let remaining = value["matches"].take();
        let matches = remaining.as_array().context("missing grep records")?;
        let page = |count: usize| {
            let mut result = value.clone();
            result["matches"] = json!(&matches[..count]);
            let next = (count < matches.len()).then_some(request.offset + count);
            result["response_page"] = json!({"offset":request.offset,"returned":count,
                "next_offset":next,"as_of_rev":commit.id().to_string(),"window_end":next.is_none()});
            result.to_string()
        };
        let full = page(matches.len());
        if full.len() <= limit {
            return Ok(full);
        }
        let minimum = usize::from(!matches.is_empty());
        let needed = page(minimum).len();
        ensure!(
            needed <= limit,
            "pinned grep needs at least {} estimated tokens for a complete match and its revision",
            needed.div_ceil(4)
        );
        let (mut low, mut high) = (minimum, matches.len());
        while low < high {
            let mid = low + (high - low).div_ceil(2);
            if page(mid).len() <= limit {
                low = mid;
            } else {
                high = mid - 1;
            }
        }
        return Ok(page(low));
    }
    let out = serde_json::to_string(&value)?;
    ensure!(
        out.len() <= limit,
        "pinned evidence requires {} estimated tokens; narrow the scope, reduce the result cap or raise max_tokens",
        out.len().div_ceil(4)
    );
    Ok(out)
}

fn scan(
    repo: &Repository,
    files: &[(String, Oid)],
    partial: &mut bool,
    scope: impl Fn(&str) -> bool,
    mut visit: impl FnMut(&str, &str) -> Result<bool>,
) -> Result<()> {
    let odb = repo.odb()?;
    let mut read_bytes = 0;
    for (path, id) in files.iter().filter(|(p, _)| scope(p)) {
        let (size, _) = odb.read_header(*id)?;
        if size > FILE_LIMIT {
            *partial = true;
            continue;
        }
        if read_bytes + size > 512 * 1024 * 1024 {
            *partial = true;
            break;
        }
        let blob = repo.find_blob(*id)?;
        if blob.is_binary() {
            continue;
        }
        read_bytes += size;
        if let Ok(text) = std::str::from_utf8(blob.content()) {
            if !visit(path, text)? {
                *partial = true;
                break;
            }
        } else {
            *partial = true;
        }
    }
    Ok(())
}

pub(super) fn lock_primitives(query: &str) -> Option<&'static str> {
    let words = query.to_lowercase();
    if ["lock", "race", "concurren", "serializ", "atomic"]
        .iter()
        .any(|w| words.contains(w))
        && [
            "sql",
            "database",
            "row",
            "write",
            "transaction",
            "update",
            "insert",
            "lock hint",
        ]
        .iter()
        .any(|w| words.contains(w))
    {
        Some(
            "UPDLOCK|HOLDLOCK|XLOCK|FOR UPDATE|BEGIN IMMEDIATE|BeginTransaction|TransactionScope|ISOLATION LEVEL|IsolationLevel",
        )
    } else {
        None
    }
}

/// A keyword can identify a candidate chunk, but only the statement is evidence.
pub(super) fn lock_witness(line: &str) -> Option<usize> {
    static HINT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(||
        regex::Regex::new(r"(?i)\b(?:updlock|holdlock|xlock|for\s+update|begin\s+immediate|isolation\s+level\s+serializable|isolationlevel\s*\.\s*serializable)\b").unwrap());
    static TRANSACTION: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(||
        regex::Regex::new(r"(?i)\b(?:BeginTransaction\s*\(|(?:new|using)\s+(?:[\w.]+\.)?TransactionScope\b)").unwrap());
    if ["'", "//", "--", "#", "/*", "*"].iter().any(|prefix| line.trim_start().starts_with(prefix)) {
        return None;
    }
    let hints = HINT.find_iter(line).count();
    (hints > 0 || TRANSACTION.is_match(line)).then_some(100 + hints * 10)
}

fn source_sites(
    repo: &Repository,
    files: &[(String, Oid)],
    query: &Query,
    partial: &mut bool,
) -> Result<Value> {
    let (term, limit, is_pattern) = match query {
        Query::References(r) => {
            ensure!(
                r.file_scope.is_none() && r.edge_kind_filter.is_none(),
                "historical references provide source occurrences; graph definition scope and edge-kind filters require a matching indexed snapshot"
            );
            (r.symbol_name.as_str(), r.max_incoming.min(500), false)
        }
        Query::Pattern(r) => {
            ensure!(
                r.response_id.is_none() && r.response_offset == 0,
                "use the saved report without as_of_rev for recovery"
            );
            (r.pattern_query.as_str(), r.max_examples.clamp(1, 20), true)
        }
        Query::Footprint(r) => {
            ensure!(
                r.response_id.is_none() && r.response_offset == 0,
                "use the saved report without as_of_rev for recovery"
            );
            (r.concept.as_str(), r.max_per_group.clamp(1, 500), false)
        }
        _ => bail!("invalid historical source query"),
    };
    ensure!(!term.trim().is_empty(), "missing search term");
    let mut names = std::collections::BTreeSet::from([term.to_owned()]);
    if !is_pattern {
        scan(
            repo,
            files,
            partial,
            |p| p.to_ascii_lowercase().ends_with(".dbml"),
            |path, text| {
                names.extend(crate::services::schema_query::mapped_spellings(
                    path, text, term,
                )?);
                Ok(true)
            },
        )?;
    }
    let terms: Vec<_> = term
        .split_whitespace()
        .filter(|w| w.len() > 2)
        .map(str::to_lowercase)
        .collect();
    let lock_query = is_pattern && lock_primitives(term).is_some();
    let pattern = if is_pattern {
        lock_primitives(term).map(str::to_owned).unwrap_or_else(|| {
            terms
                .iter()
                .map(|w| regex::escape(w))
                .collect::<Vec<_>>()
                .join("|")
        })
    } else {
        names
            .iter()
            .map(|w| regex::escape(w))
            .collect::<Vec<_>>()
            .join("|")
    };
    ensure!(!pattern.is_empty(), "no searchable terms");
    let re = regex::RegexBuilder::new(&pattern)
        .case_insensitive(true)
        .build()?;
    let mut best = std::collections::BTreeSet::new();
    let mut total = 0usize;
    scan(
        repo,
        files,
        partial,
        |path| !is_pattern || implementation_file(path),
        |path, text| {
            if !re.is_match(text) {
                return Ok(true);
            }
            let mut effects = std::collections::BTreeMap::new();
            if !is_pattern && path.to_ascii_lowercase().ends_with(".vb") {
                for site in engram_index::host_sql::table_sites(text, 1) {
                    if names
                        .iter()
                        .any(|name| name.eq_ignore_ascii_case(&site.table))
                    {
                        effects.insert(site.line as usize, (site.access, site.operation));
                    }
                }
            }
            let mut exemplar = None;
            for (idx, line) in text.lines().enumerate() {
                let number = idx + 1;
                if !effects.contains_key(&number) && !re.is_match(line) {
                    continue;
                }
                let trimmed = line.trim();
                let lower = trimmed.to_lowercase();
                let (score, label) = if let Some((access, op)) = effects.get(&number) {
                    (100, format!("SQL {access}: {op}"))
                } else if lock_query {
                    let Some(score) = lock_witness(line) else { continue; };
                    (score, "implementation candidate".into())
                } else if is_pattern {
                    if ["'", "//", "--", "#", "/*", "*"]
                        .iter()
                        .any(|start| trimmed.starts_with(start))
                    {
                        continue;
                    }
                    let hints = [
                        "updlock",
                        "holdlock",
                        "xlock",
                        "for update",
                        "begin immediate",
                    ];
                    (
                        1 + terms.iter().filter(|w| lower.contains(w.as_str())).count()
                            + 10 * hints.iter().filter(|w| lower.contains(**w)).count(),
                        "implementation candidate".into(),
                    )
                } else {
                    (1, "source occurrence".into())
                };
                let entry = (
                    std::cmp::Reverse(score),
                    path.to_owned(),
                    number,
                    line.to_owned(),
                    label,
                );
                if is_pattern {
                    if exemplar.as_ref().is_none_or(|old| entry < *old) {
                        exemplar = Some(entry);
                    }
                } else {
                    total += 1;
                    best.insert(entry);
                    if best.len() > limit {
                        best.pop_last();
                    }
                }
            }
            if let Some(entry) = exemplar {
                total += 1;
                best.insert(entry);
                if best.len() > limit {
                    best.pop_last();
                }
            }
            Ok(true)
        },
    )?;
    let sites:Vec<_>=best.into_iter().map(|(_,path,line,text,kind)|
        json!({"file_path":path,"line":line,"line_text":text,"evidence":kind})).collect();
    Ok(
        json!({"source_sites":sites,"total_candidates":total,"result_limit":limit,"mapped_names":names,
        "coverage":"Source occurrences, DBML names and literal SQL evidence at this revision. Historical caller binding and co-change scores are unavailable."}),
    )
}

pub(super) fn implementation_file(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    !engram_core::is_vendor_path(path)
        && !lower.ends_with(".d.ts")
        && [
            "vb", "cs", "sql", "java", "py", "ts", "js", "tsx", "jsx", "go", "rs", "kt", "php",
            "rb", "c", "h", "cpp", "swift", "scala", "aspx", "ascx", "html", "vue", "svelte", "ex",
            "exs", "fs", "lua", "pl", "sh",
        ]
        .contains(&lower.rsplit('.').next().unwrap_or(""))
}
