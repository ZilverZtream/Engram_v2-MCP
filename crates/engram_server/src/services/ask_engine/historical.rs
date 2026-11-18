//! Read source at one resolved Git revision. No active index, live graph,
//! working-tree bytes or current policy may leak into this evidence report.
//! Applicable guidance is read from the same immutable Git revision.
use super::{
    evidence::{Authority, EvidenceItem, EvidenceKind},
    plan::{EntityKind, QueryPlan},
    report::AskReport,
    status::{AnswerStatus, FreshnessSnapshot, ProviderReport, ProviderStatus},
};
use crate::models::{AsOf, AskCodebaseRequest};
use anyhow::{Context, Result, ensure};
use git2::{ObjectType, Repository, TreeWalkMode, TreeWalkResult};
use std::{
    cell::Cell,
    collections::BTreeSet,
    path::Path,
    time::{Duration, Instant},
};

const BLOB_LIMIT: usize = 128 * 1024;
const TOTAL_LIMIT: usize = 2 * 1024 * 1024;
const ENTRY_LIMIT: usize = 2048;
const BLOB_SCAN_LIMIT: usize = 128;

fn revision(repo: &Repository, pin: &AsOf) -> Result<git2::Oid> {
    let commit = pin
        .commit
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let branch = pin
        .branch
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    ensure!(
        commit.is_some() || branch.is_some(),
        "as_of requires a nonempty commit or branch"
    );
    ensure!(
        [commit, branch]
            .into_iter()
            .flatten()
            .all(|s| s.len() <= 256 && !s.chars().any(char::is_control)),
        "Invalid as_of revision"
    );
    let from_commit = commit
        .map(|name| repo.revparse_single(name)?.peel_to_commit().map(|c| c.id()))
        .transpose()
        .context("Requested Git commit is unavailable; current source was not substituted")?;
    let from_branch = branch
        .map(|name| {
            repo.find_branch(name, git2::BranchType::Local)
                .or_else(|_| repo.find_branch(name, git2::BranchType::Remote))?
                .get()
                .peel_to_commit()
                .map(|c| c.id())
        })
        .transpose()
        .context("Requested Git branch is unavailable; current source was not substituted")?;
    if let (Some(commit), Some(branch)) = (from_commit, from_branch) {
        ensure!(
            commit == branch,
            "as_of commit and branch resolve to different revisions"
        );
    }
    Ok(from_commit.or(from_branch).expect("nonempty pin checked"))
}

fn scope_matches(path: &str, plan: &QueryPlan) -> bool {
    plan.qualifiers.path_prefixes.is_empty()
        || plan.qualifiers.path_prefixes.iter().any(|prefix| {
            let prefix = prefix.trim_matches('/').to_ascii_lowercase();
            let path = path.to_ascii_lowercase();
            path == prefix || path.starts_with(&format!("{prefix}/"))
        })
}

/// Return complete bounded UTF-8 blobs. Existing whole-report continuation
/// owns exact recovery, so no action accidentally opens today's file instead.
pub fn gather(root: &Path, req: &AskCodebaseRequest, plan: QueryPlan) -> Result<AskReport> {
    let started = Instant::now();
    let deadline = Duration::from_millis(req.deadline_ms.unwrap_or(15_000).clamp(1_000, 60_000));
    let repo = Repository::discover(root)
        .context("Historical answers require the project's Git repository")?;
    let oid = revision(&repo, req.as_of.as_ref().context("Missing as_of revision")?)?;
    let commit = repo.find_commit(oid)?;
    let workdir = std::fs::canonicalize(
        repo.workdir()
            .context("Historical project must have a working directory")?,
    )?;
    let canonical_root = std::fs::canonicalize(root)?;
    let prefix = canonical_root
        .strip_prefix(&workdir)
        .context("Project is outside its Git working directory")?;
    let full_tree = commit.tree()?;
    let tree = if prefix.as_os_str().is_empty() {
        full_tree
    } else {
        repo.find_tree(
            full_tree
                .get_path(prefix)
                .context("Project directory is absent at the requested revision")?
                .id(),
        )?
    };
    let odb = repo.odb()?;
    let explicit: BTreeSet<_> = plan
        .entities
        .iter()
        .filter(|entity| entity.guessed_kind == EntityKind::File)
        .map(|entity| entity.text.replace('\\', "/"))
        .collect();
    let terms: BTreeSet<_> = plan
        .entities
        .iter()
        .filter(|entity| entity.guessed_kind != EntityKind::File)
        .map(|entity| entity.text.to_lowercase())
        .filter(|text| text.len() >= 3)
        .collect();
    let terms = if terms.is_empty() {
        req.question
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|word| word.len() >= 4)
            .map(str::to_lowercase)
            .filter(|word| {
                ![
                    "what", "does", "which", "where", "explain", "function", "method", "return",
                    "under", "from", "this", "that", "with", "have", "code",
                ]
                .contains(&word.as_str())
            })
            .collect()
    } else {
        terms
    };
    let result_limit = (match req.depth.as_str() {
        "quick" => 2,
        "deep" => 10,
        _ => 6,
    })
    .max(explicit.len().min(8));
    let mut examined = 0usize;
    let mut entries = 0usize;
    let mut bytes = 0usize;
    let limited = Cell::new(false);
    let mut unknowns = vec!["Historical source retrieval uses immutable Git blobs at the resolved revision. No historical call graph, registry decisions, generated business rules, semantic embeddings, runtime execution or accepted policy intent is inferred from current state.".into()];
    let mut candidates = Vec::<(usize, String, String, String)>::new();
    let mut seen = BTreeSet::new();
    let mut failure = None;
    let direct_paths = !explicit.is_empty()
        && explicit.len() <= 8
        && explicit
            .iter()
            .all(|path| path.contains('/') && tree.get_path(Path::new(path)).is_ok());
    // Exact question paths are read first, so unrelated directory ordering
    // cannot consume the scan budget before the explicitly requested file.
    let mut inspect = |path: String, id: git2::Oid, exact: bool| -> Result<()> {
        if !scope_matches(&path, &plan) || !seen.insert(path.clone()) {
            return Ok(());
        }
        if examined == BLOB_SCAN_LIMIT || started.elapsed() >= deadline {
            limited.set(true);
            return Ok(());
        }
        examined += 1;
        let (size, kind) = odb.read_header(id)?;
        if kind != ObjectType::Blob {
            return Ok(());
        }
        if size > BLOB_LIMIT || size > TOTAL_LIMIT.saturating_sub(bytes) {
            limited.set(true);
            if unknowns.len() < 16 {
                unknowns.push(format!(
                    "{path}: complete historical blob omitted at the128KiB per-blob or2MiB total read limit"
                ));
            }
            return Ok(());
        }
        let blob = repo.find_blob(id)?;
        bytes += size;
        if blob.content().contains(&0) {
            return Ok(());
        }
        let Ok(content) = std::str::from_utf8(blob.content()) else {
            if unknowns.len() < 16 {
                unknowns.push(format!(
                    "{path}: historical bytes are not UTF-8; no lossy source evidence returned"
                ));
            }
            return Ok(());
        };
        let lower = content.to_lowercase();
        let score = if exact {
            usize::MAX
        } else {
            terms
                .iter()
                .filter(|term| {
                    lower.contains(term.as_str()) || path.to_lowercase().contains(term.as_str())
                })
                .count()
        };
        if score > 0 {
            candidates.push((score, path, content.to_owned(), id.to_string()));
            // Keep only a bounded ranked set; explicit files precede lexical
            // candidates. Discarding a candidate is disclosed as partial coverage.
            candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
            if candidates.len() > result_limit {
                candidates.pop();
                limited.set(true);
            }
        }
        Ok(())
    };
    for path in explicit.iter().take(8) {
        if path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        {
            continue;
        }
        if let Ok(entry) = tree.get_path(Path::new(path)) {
            if matches!(entry.filemode(), 0o100644 | 0o100755) {
                inspect(path.clone(), entry.id(), true)?;
            }
        }
    }
    if explicit.len() > 8 {
        limited.set(true);
    }
    let walk = if direct_paths {
        Ok(())
    } else {
        tree.walk(TreeWalkMode::PreOrder, |directory, entry| {
            entries += 1;
            if entries > ENTRY_LIMIT || started.elapsed() >= deadline {
                limited.set(true);
                return TreeWalkResult::Abort;
            }
            let Some(name) = entry.name() else {
                return TreeWalkResult::Skip;
            };
            let path = format!("{directory}{name}");
            if !matches!(entry.filemode(), 0o100644 | 0o100755) {
                return TreeWalkResult::Ok;
            }
            if !explicit.is_empty()
                && !explicit.iter().any(|query| {
                    path.eq_ignore_ascii_case(query)
                        || path
                            .to_ascii_lowercase()
                            .ends_with(&format!("/{}", query.to_ascii_lowercase()))
                })
            {
                return TreeWalkResult::Ok;
            }
            if let Err(error) = inspect(path, entry.id(), !explicit.is_empty()) {
                failure = Some(error);
                return TreeWalkResult::Abort;
            }
            TreeWalkResult::Ok
        })
    };
    drop(inspect);
    if let Some(error) = failure {
        return Err(error);
    }
    if let Err(error) = walk {
        if !limited.get() {
            return Err(error.into());
        }
    }
    if limited.get() {
        unknowns.push("Historical retrieval reached its bounded scan/read/result/deadline limits; evidence is a selected set, never an exhaustive absence or caller proof.".into());
    }
    let evidence: Vec<_> = candidates.into_iter().enumerate().map(|(i,(_,path,content,blob))| {
        let hash = blake3::hash(content.as_bytes()).to_hex().to_string();
        let lines = content.lines().count().max(1) as u32;
        EvidenceItem {
            memory_record: None,
            evidence_id:format!("ev_{}",i+1),document_id:None,document_namespace:None,
            source_verification:Some("immutable_git_blob_at_requested_revision".into()),
            kind:if path.ends_with(".md") || path.ends_with(".txt") { EvidenceKind::DocSection } else { EvidenceKind::SourceCode },
            authority:Authority::HistoricalCode,path:Some(path),lines:Some((1,lines)),symbol_id:None,title:None,
            content,generation:None,commit:Some(oid.to_string()),timestamp:None,confidence:1.0,relevance:1.0,
            extraction_method:"git_blob".into(),warnings:vec![format!("source_blake3={hash}; git_blob_oid={blob}; complete admitted historical blob. Source definitions do not prove execution or successful deployment. Recover this saved report rather than opening the current working-tree file.")],
            provider:"historical_source".into(),score:None,directness:None,
        }
    }).collect();
    let count = evidence.len();
    let policy_paths: Vec<_> = evidence.iter()
        .filter(|item| item.kind == EvidenceKind::SourceCode)
        .filter_map(|item| item.path.clone())
        .collect::<BTreeSet<_>>().into_iter().collect();
    let mut policy = if policy_paths.is_empty() {
        let mut withheld = crate::services::project_policy::PolicyContext::historical_unavailable();
        withheld.notes.push("Historical policy scope was not evaluated because no code paths were returned. No task scope or live guidance was substituted.".into());
        withheld
    } else {
        match crate::services::project_policy::resolve_at_revision(root, oid, &policy_paths, &[]) {
            Ok(context) => context,
            Err(error) => {
                let mut withheld = crate::services::project_policy::PolicyContext::historical_unavailable();
                withheld.notes.push(format!("Guidance at resolved revision {oid} could not be read: {error:#}. Working-tree policy was not substituted."));
                withheld
            }
        }
    };
    policy.source_revision = Some(oid.to_string());
    policy.notes.push("Policy scope covers only the code paths returned in this report at source_revision; it does not establish reviewer approval, current policy, implementation compliance or the scope of a future edit.".into());
    if policy.coverage != "complete" {
        unknowns.push("Historical authored guidance is incomplete or not evaluated; inspect project_policy_sources omissions and notes. Code evidence remains revision-pinned and policy coverage does not upgrade the answer status.".into());
    }
    Ok(AskReport {
        question:req.question.clone(),plan,status:if count > 0 { AnswerStatus::Partial } else { AnswerStatus::Unsupported },
        mode:"retrieval_only".into(),synthesis:None,
        project_policy_sources:Some(policy),
        evidence,conflicts:Vec::new(),unknowns,
        next_best:vec!["Inspect the complete source at the cited Git revision. Do not use current index/graph results to fill historical behavior or policy gaps.".into()],
        snapshot:FreshnessSnapshot {git_commit:Some(oid.to_string()),git_branch:req.as_of.as_ref().and_then(|pin|pin.branch.as_deref()).map(str::trim).filter(|name|!name.is_empty()).map(str::to_owned),semantic_tier:"off_historical_git_source".into(),..Default::default()},
        providers:vec![ProviderReport {provider:"historical_source".into(),status:if count > 0 { ProviderStatus::Hit } else { ProviderStatus::Empty },count,
            note:Some(format!("Resolved one immutable revision; examined {examined} budget-checked candidate blobs, read {bytes} bytes, visited {entries} tree entries. Current index and working-tree evidence were not consulted.")),
            examined,available:None,truncated:limited.get(),proof:None}],answer_members:Vec::new(),
    })
}
