use std::{path::Path, time::Duration};

use engram_git::history::GitWalker;
use rmcp::ErrorData as McpError;
use serde_json::json;

use crate::models::AskCodebaseRequest;
use crate::services::{
    ask_engine::{
        evidence::{Authority, EvidenceItem, EvidenceKind},
        plan::{AnswerType, EntityKind, EntityMention, Intent, Qualifiers, QueryPlan},
        report::AskReport,
        retrieval::parse_depth,
        status::{AnswerStatus, FreshnessSnapshot, ProviderReport, ProviderStatus},
    },
    change_card_service::{CardFilter, recent_window},
};

const MAX_COMMITS: usize = 1000;
const PROVIDER: &str = "recent_git_history";

pub(super) fn requested_path(question: &str) -> Option<String> {
    let question = question.trim();
    let question = question.strip_suffix('?').unwrap_or(question).trim_end();
    let lower = question.to_ascii_lowercase();
    let path = [
        "what changed recently in ",
        "what changed recently to ",
        "show recent changes in ",
        "show recent changes to ",
    ]
    .into_iter()
    .find_map(|prefix| {
        lower
            .starts_with(prefix)
            .then(|| question[prefix.len()..].trim())
    })?;
    let path = if let Some(quoted) = path.strip_prefix('\x60') {
        quoted.strip_suffix('\x60')?
    } else {
        if path.chars().any(char::is_whitespace) {
            return None;
        }
        path
    };
    let path = path.replace('\\', "/");
    let normalized = path.strip_suffix('/').unwrap_or(&path);
    let has_file_extension = Path::new(normalized)
        .extension()
        .is_some_and(|extension| !extension.is_empty());
    if path.len() > 1024
        || path.contains([':', '?', '\x60'])
        || path.chars().any(char::is_control)
        || normalized.split('/').any(|part| matches!(part, "" | "." | ".."))
        || !(has_file_extension || path.contains('/'))
    {
        return None;
    }
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::requested_path;

    #[test]
    fn recognizes_literal_history_paths_without_routing_generic_questions() {
        for (question, path) in [
            ("What changed recently in Site/Reports/?", "Site/Reports/"),
            ("Show recent changes in Site\\Reports?", "Site/Reports"),
            ("What changed recently to `Site/Report Views/`?", "Site/Report Views/"),
            ("Show recent changes in Site/Reports/Page.aspx?", "Site/Reports/Page.aspx"),
        ] {
            assert_eq!(requested_path(question).as_deref(), Some(path));
        }
        for question in [
            "What changed recently in reporting?",
            "Show recent changes in /Site/Reports/",
            "Show recent changes in Site/../Reports/",
        ] {
            assert_eq!(requested_path(question), None);
        }
    }
}

pub(super) async fn gather(
    req: &AskCodebaseRequest,
    directory: &str,
    path: String,
) -> Result<AskReport, McpError> {
    let root = std::path::PathBuf::from(directory);
    let limit = parse_depth(&req.depth).arm_top_k();
    let project_path = path.clone();
    let lookup = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let repo = GitWalker::open_repo(&root)?;
        let workdir = repo
            .workdir()
            .ok_or_else(|| anyhow::anyhow!("Recent history requires a Git working directory"))?
            .canonicalize()?;
        let canonical_root = root.canonicalize()?;
        let prefix = canonical_root.strip_prefix(&workdir)?;
        let git_path = prefix.join(project_path);
        let git_path = git_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Git path must be UTF-8"))?
            .replace('\\', "/");
        let (tip, basis) = match GitWalker::approved_history_root(&repo) {
            Some(tip) => (tip, "origin default branch"),
            None => (repo.head()?.peel_to_commit()?.id(), "checked-out branch"),
        };
        let filter = CardFilter {
            file_prefix: Some(git_path.clone()),
            excluded_prefixes: Vec::new(),
            author: None,
            date_after: None,
            date_before: None,
        };
        let window = recent_window(&root, tip, &filter, MAX_COMMITS, limit)?;
        Ok((tip, basis, git_path, window))
    });
    let deadline = Duration::from_millis(req.deadline_ms.unwrap_or(15_000).clamp(1_000, 60_000));
    let (tip, basis, git_path, window) = tokio::time::timeout(deadline, lookup)
        .await
        .map_err(|_| {
            McpError::internal_error("Recent Git history exceeded the request deadline", None)
        })?
        .map_err(|error| McpError::internal_error(error.to_string(), None))?
        .map_err(|error| {
            McpError::internal_error(format!("Recent Git history: {error:#}"), None)
        })?;

    let mut next_best = Vec::new();
    let mut evidence = Vec::new();
    for card in window.cards.iter().rev() {
        if !path.ends_with('/') && card.files.contains(&git_path) {
            let patch = json!({
                "project_id": req.project_id, "mode": "patch", "query": card.oid,
                "file_filter": git_path, "as_of_rev": tip.to_string(), "output_json": true
            });
            next_best.push(format!("Inspect the file diff with search_history({patch})"));
        } else {
            let commit = json!({
                "project_id": req.project_id, "mode": "commit", "query": card.oid,
                "as_of_rev": tip.to_string(), "output_json": true
            });
            next_best.push(format!("Inspect the complete change with search_history({commit})"));
        }
        evidence.push(EvidenceItem {
            evidence_id: format!("ev_{}", evidence.len() + 1),
            kind: EvidenceKind::HistoryCommit,
            authority: Authority::RecordedHistory,
            path: Some(path.clone()),
            lines: None,
            symbol_id: None,
            title: Some(card.title.clone()),
            content: format!(
                "Commit {} by {} (Unix time {}). {} matching changed paths. Showing {}.",
                card.oid,
                card.author,
                card.timestamp,
                card.file_count,
                card.files.join(", ")
            ),
            generation: None,
            commit: Some(card.oid.clone()),
            timestamp: Some(card.timestamp),
            confidence: 1.0,
            relevance: 1.0,
            extraction_method: "git".into(),
            provider: PROVIDER.into(),
            memory_record: None,
            document_id: None,
            document_namespace: None,
            source_verification: None,
            warnings: Vec::new(),
            score: None,
            directness: None,
        });
    }
    let mut unknowns = Vec::new();
    if let Some(next) = window.next_revision {
        unknowns.push("Older first-parent history was outside this window.".into());
        let older = json!({
            "project_id": req.project_id, "mode": "recent", "query": "",
            "file_filter": git_path, "as_of_rev": next.to_string(), "output_json": true
        });
        next_best.push(format!("Continue with search_history({older})"));
    }
    let count = evidence.len();
    if count == 0 {
        unknowns.push(format!(
            "No changes to {path} were found in the scanned history."
        ));
    }
    Ok(AskReport {
        question: req.question.clone(),
        plan: QueryPlan {
            model_routing: None,
            intents: vec![(Intent::History, 1.0)],
            entities: vec![EntityMention {
                text: path,
                guessed_kind: EntityKind::File,
                resolved: Vec::new(),
            }],
            qualifiers: Qualifiers::default(),
            needed_evidence: vec![EvidenceKind::HistoryCommit],
            answer_type: AnswerType::Timeline,
            modalities: Vec::new(),
            contract: Default::default(),
        },
        status: if count == 0 {
            AnswerStatus::Partial
        } else {
            AnswerStatus::Answered
        },
        mode: "retrieval_only".into(),
        synthesis: None,
        project_policy_sources: None,
        evidence,
        conflicts: Vec::new(),
        unknowns,
        next_best,
        snapshot: FreshnessSnapshot {
            git_commit: Some(tip.to_string()),
            semantic_tier: "off".into(),
            ..Default::default()
        },
        providers: vec![ProviderReport {
            provider: PROVIDER.into(),
            status: if count == 0 {
                ProviderStatus::Empty
            } else {
                ProviderStatus::Hit
            },
            count,
            note: Some(format!(
                "Newest-first commit summaries from the {basis}. First-parent scan capped at {MAX_COMMITS} commits and {limit} matching results. Follow the file patch or complete commit for exact changes."
            )),
            examined: window.commits_scanned,
            available: None,
            truncated: window.next_revision.is_some(),
            proof: None,
        }],
        answer_members: Vec::new(),
    })
}
