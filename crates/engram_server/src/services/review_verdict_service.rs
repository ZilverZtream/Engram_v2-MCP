//! Review-verdict memory: what the people who decide said about past review
//! findings, and why.
//!
//! A pull-request thread's status is not its verdict. Teams close a finding
//! as `fixed` while the decision-maker's reply says "this is per design", and
//! mark it `wontFix` after a decision-maker endorsed it. The learning an
//! agent needs — which findings the team rejects, with the reason, and which
//! it insists on — lives in the replies and reactions. This module turns a
//! thread into one verdict with its evidence:
//!
//! 1. A decision-maker's reply (latest decisive one) — rejected, deferred or
//!    endorsed, with the reply quoted as the reasoning.
//! 2. A decision-maker's thumbs-up on the finding — endorsed.
//! 3. A finding raised by a decision-maker — the team was asked to act.
//! 4. The PR author's pushback when it carries an argument — contested, with
//!    the argument quoted; a decision-maker authoring the PR decides.
//! 5. Status alone — a bare `wontFix` teaches nothing and suppresses nothing.
//!
//! Decision-makers are configuration, never names in source.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::LazyLock;

/// One comment in a review thread.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReviewComment {
    pub author: String,
    pub text: String,
    /// Display names of users who liked (thumbs-up) this comment.
    #[serde(default)]
    pub likes: Vec<String>,
}

/// One review thread on a pull request; the first comment is the finding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReviewThread {
    pub pr_id: u64,
    #[serde(default)]
    pub pr_title: String,
    pub pr_author: String,
    /// ISO-8601 close (or creation) date of the pull request.
    #[serde(default)]
    pub pr_date: String,
    pub thread_id: u64,
    /// Provider status: fixed, wontFix, byDesign, closed, active, ...
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub file_path: String,
    #[serde(default)]
    pub line: u32,
    pub comments: Vec<ReviewComment>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictKind {
    /// A decision-maker rejected the finding.
    Rejected,
    /// A decision-maker accepted it as valid but moved it elsewhere.
    Deferred,
    /// A decision-maker endorsed or confirmed it.
    Endorsed,
    /// A decision-maker raised it.
    RaisedByDecisionMaker,
    /// The PR author declined it with an argument; no decision-maker ruled.
    ContestedByAuthor,
    /// Declined by status only — no reason given.
    DismissedWithoutReason,
    /// Marked fixed; no decision-maker signal.
    Fixed,
    /// Still open.
    Open,
}

impl VerdictKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rejected => "rejected",
            Self::Deferred => "deferred",
            Self::Endorsed => "endorsed",
            Self::RaisedByDecisionMaker => "raised_by_decision_maker",
            Self::ContestedByAuthor => "contested_by_author",
            Self::DismissedWithoutReason => "dismissed_without_reason",
            Self::Fixed => "fixed",
            Self::Open => "open",
        }
    }
}

/// A thread's verdict and the evidence it rests on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    pub kind: VerdictKind,
    /// Who decided (a decision-maker, or the arguing PR author).
    pub decided_by: Option<String>,
    /// Which rule produced it: decision_maker_reply, decision_maker_like,
    /// decision_maker_finding, decision_maker_author, author_argument, status.
    pub basis: String,
    /// The quoted reply or argument, markup stripped.
    pub reasoning: Option<String>,
}

/// Words a PR author's reply needs before it counts as an argument.
pub const ARGUMENT_MIN_WORDS: usize = 25;

fn patterns(list: &[&str]) -> Vec<Regex> {
    list.iter()
        .map(|p| Regex::new(&format!("(?i){p}")).expect("verdict pattern"))
        .collect()
}

static LEADS_FIXED: LazyLock<Vec<Regex>> =
    LazyLock::new(|| patterns(&[r"^\W*(fixed|confirmed|resolved|addressed|done)\b"]));
static DEFERRED: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    patterns(&[
        r"\bout of scope\b",
        r"\banother (us|user story|story|pr)\b",
        r"\bseparate(ly)? (us|story|pr|ticket)\b",
        r"\bhandled separately\b",
        // Markup is stripped first, so a work-item link survives only as text.
        r"work ?items?/edit/\d+",
        r"work item created",
        r"\buser story #?\d+",
        r"\bpredates this pr\b",
        r"\bfollow[- ]up\b",
        r"\blater (us|story|pr)\b",
        r"\bwhole code ?base\b",
        r"\bsoon be addressed\b",
    ])
});
static REJECTED: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    patterns(&[
        r"\bper design\b",
        r"\bby design\b",
        r"\bas designed\b",
        r"\bintentional",
        r"\bacceptable\b",
        r"\bis accepted\b",
        r"\bnot (a )?valid\b",
        r"\bnot applicable\b",
        r"\bdoes not reproduce\b",
        r"\bnot supported by\b",
        r"\bwe will not\b",
        r"\bwon'?t fix\b",
        r"\bno need\b",
        r"\bnot needed\b",
        r"\bnot necessary\b",
        r"\bintended behaviou?r\b",
        r"\bthis is fine\b",
        r"\b(keep|leave) (it|this) as\b",
        r"\bfalse positive\b",
        r"\bwithdr[ae]w\b",
        r"\bskip\b",
        r"\bnot an issue\b",
        r"\bhave nothing to do with\b",
        r"\bnot relevant\b",
    ])
});
static ENDORSED: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    patterns(&[
        r"\bfixed in\b",
        r"\bresolved in code\b",
        r"\baddressed in (commit|c)",
        r"\bconfirmed valid\b",
        r"\bgood catch\b",
        r"\bplease (fix|address)\b",
        r"\bshould be fixed\b",
        r"\bmust be (fixed|addressed)\b",
        r"\bagree\b",
    ])
});

/// What a decision-maker's reply decides, if anything. An explicit
/// "Fixed…/Confirmed…" opening decides; otherwise a deferral or rejection
/// anywhere outranks a passing mention of a fix.
pub fn classify_reply(text: &str) -> Option<VerdictKind> {
    let text = plain_text(text);
    let any = |set: &[Regex]| set.iter().any(|p| p.is_match(&text));
    if any(&LEADS_FIXED) {
        Some(VerdictKind::Endorsed)
    } else if any(&DEFERRED) {
        Some(VerdictKind::Deferred)
    } else if any(&REJECTED) {
        Some(VerdictKind::Rejected)
    } else if any(&ENDORSED) {
        Some(VerdictKind::Endorsed)
    } else {
        None
    }
}

/// Comment text without HTML/markdown markup, whitespace collapsed.
pub fn plain_text(text: &str) -> String {
    static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]*>").expect("tag"));
    let without_tags = TAG.replace_all(text, " ");
    without_tags
        .replace(['*', '`', '_', '#'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn word_count(text: &str) -> usize {
    plain_text(text)
        .split_whitespace()
        .filter(|w| w.chars().any(char::is_alphanumeric))
        .count()
}

fn is_one_of(name: &str, people: &[String]) -> bool {
    people.iter().any(|p| p.eq_ignore_ascii_case(name.trim()))
}

fn declined(status: &str) -> bool {
    matches!(status, "wontFix" | "byDesign" | "closed")
}

/// The verdict of one thread under the rules in the module docs.
pub fn thread_verdict(thread: &ReviewThread, decision_makers: &[String]) -> Verdict {
    let verdict = |kind, decided_by: Option<&str>, basis: &str, reasoning: Option<&str>| Verdict {
        kind,
        decided_by: decided_by.map(str::to_string),
        basis: basis.to_string(),
        reasoning: reasoning.map(plain_text),
    };
    let Some(finding) = thread.comments.first() else {
        return verdict(VerdictKind::Open, None, "status", None);
    };
    let replies = &thread.comments[1..];
    if let Some((reply, kind)) = replies
        .iter()
        .filter(|c| is_one_of(&c.author, decision_makers))
        .filter_map(|c| classify_reply(&c.text).map(|kind| (c, kind)))
        .last()
    {
        return verdict(
            kind,
            Some(&reply.author),
            "decision_maker_reply",
            Some(&reply.text),
        );
    }
    if let Some(liker) = finding.likes.iter().find(|u| is_one_of(u, decision_makers)) {
        return verdict(
            VerdictKind::Endorsed,
            Some(liker),
            "decision_maker_like",
            None,
        );
    }
    if is_one_of(&finding.author, decision_makers) {
        return verdict(
            VerdictKind::RaisedByDecisionMaker,
            Some(&finding.author),
            "decision_maker_finding",
            None,
        );
    }
    let argument = replies
        .iter()
        .rev()
        .find(|c| c.author == thread.pr_author && word_count(&c.text) >= ARGUMENT_MIN_WORDS);
    match (declined(&thread.status), argument) {
        (true, Some(arg)) if is_one_of(&thread.pr_author, decision_makers) => verdict(
            VerdictKind::Rejected,
            Some(&arg.author),
            "decision_maker_author",
            Some(&arg.text),
        ),
        (true, Some(arg)) => verdict(
            VerdictKind::ContestedByAuthor,
            Some(&arg.author),
            "author_argument",
            Some(&arg.text),
        ),
        (true, None) => verdict(VerdictKind::DismissedWithoutReason, None, "status", None),
        (false, _) if thread.status == "fixed" => verdict(VerdictKind::Fixed, None, "status", None),
        _ => verdict(VerdictKind::Open, None, "status", None),
    }
}

/// How a PR author's pushback has fared: explicit declines (`wontFix` /
/// `byDesign`) on others' findings, split by whether they carried an
/// argument and whether a decision-maker later endorsed the finding anyway.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AuthorRecord {
    pub argued_stood: usize,
    pub argued_overturned: usize,
    pub bare: usize,
}

impl AuthorRecord {
    /// Share of explicit declines that carried an argument.
    pub fn argued_rate(&self) -> f64 {
        let argued = self.argued_stood + self.argued_overturned;
        argued as f64 / (argued + self.bare).max(1) as f64
    }
}

pub fn author_records(
    threads: &[ReviewThread],
    decision_makers: &[String],
) -> BTreeMap<String, AuthorRecord> {
    let mut records: BTreeMap<String, AuthorRecord> = BTreeMap::new();
    for thread in threads {
        let author = &thread.pr_author;
        let Some(finding) = thread.comments.first() else {
            continue;
        };
        if is_one_of(author, decision_makers)
            || finding.author == *author
            || !matches!(thread.status.as_str(), "wontFix" | "byDesign")
        {
            continue;
        }
        let argued = thread.comments[1..]
            .iter()
            .any(|c| c.author == *author && word_count(&c.text) >= ARGUMENT_MIN_WORDS);
        let record = records.entry(author.clone()).or_default();
        if !argued {
            record.bare += 1;
            continue;
        }
        let ruled: Vec<VerdictKind> = thread.comments[1..]
            .iter()
            .filter(|c| is_one_of(&c.author, decision_makers))
            .filter_map(|c| classify_reply(&c.text))
            .collect();
        let liked = finding.likes.iter().any(|u| is_one_of(u, decision_makers));
        let overturned = ruled.contains(&VerdictKind::Endorsed)
            || (liked && !ruled.contains(&VerdictKind::Rejected));
        if overturned {
            record.argued_overturned += 1;
        } else {
            record.argued_stood += 1;
        }
    }
    records
}

/// The searchable text of one verdict: the finding and the reasoning carry
/// the words a future reviewer's draft will share.
pub fn verdict_doc_content(thread: &ReviewThread, verdict: &Verdict) -> String {
    const MAX: usize = 1500;
    let clip = |s: &str| -> String {
        if s.chars().count() > MAX {
            format!("{}…", s.chars().take(MAX).collect::<String>())
        } else {
            s.to_string()
        }
    };
    let finding = thread.comments.first();
    let mut out = format!(
        "Review verdict: {}{}\nPR-{}: {} | author {} | {}\n",
        verdict.kind.as_str(),
        verdict
            .decided_by
            .as_deref()
            .map(|who| format!(" by {who} ({})", verdict.basis))
            .unwrap_or_default(),
        thread.pr_id,
        thread.pr_title,
        thread.pr_author,
        thread.pr_date.get(..10).unwrap_or(&thread.pr_date),
    );
    if !thread.file_path.is_empty() {
        out.push_str(&format!("File: {}:{}\n", thread.file_path, thread.line));
    }
    if let Some(finding) = finding {
        out.push_str(&format!(
            "Finding by {}:\n{}\n",
            finding.author,
            clip(&plain_text(&finding.text))
        ));
    }
    if let Some(reasoning) = &verdict.reasoning {
        out.push_str(&format!(
            "Reasoning ({}):\n{}\n",
            verdict.decided_by.as_deref().unwrap_or("unknown"),
            clip(reasoning)
        ));
    }
    out
}

/// Which pull requests to read, bounded for leak-free historical replays.
#[derive(Debug, Clone, Default)]
pub struct FetchWindow {
    /// Inclusive lower PR id.
    pub min_pr_id: Option<u64>,
    /// Inclusive upper PR id.
    pub max_pr_id: Option<u64>,
    /// Exclusive `YYYY-MM-DD` close-date bound; undated (open) PRs are then
    /// excluded too.
    pub completed_before: Option<String>,
    /// Newest-first cap on PRs read.
    pub max_prs: Option<usize>,
}

/// Every non-system review thread of the repository's pull requests (open
/// and completed) inside `window`, with each comment's likes.
pub async fn fetch_azure_devops_threads(
    org: &str,
    project: &str,
    repo: &str,
    pat: &str,
    window: &FetchWindow,
) -> anyhow::Result<Vec<ReviewThread>> {
    use reqwest::header::{ACCEPT, AUTHORIZATION};
    let auth = format!(
        "Basic {}",
        crate::services::code_review_ingest_service::base64_encode(format!(":{pat}").as_bytes())
    );
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;
    let esc = crate::services::code_review_ingest_service::url_escape;
    let base = format!(
        "https://dev.azure.com/{}/{}/_apis/git/repositories/{}",
        esc(org),
        esc(project),
        esc(repo)
    );
    let get = |url: String| {
        let request = client
            .get(url)
            .header(AUTHORIZATION, &auth)
            .header(ACCEPT, "application/json");
        async move {
            anyhow::Ok(
                request
                    .send()
                    .await?
                    .error_for_status()?
                    .json::<serde_json::Value>()
                    .await?,
            )
        }
    };
    let text = |v: &serde_json::Value, key: &str| {
        v.get(key)
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string()
    };

    let mut prs: Vec<serde_json::Value> = Vec::new();
    for status in ["completed", "active"] {
        let mut skip = 0usize;
        loop {
            let page = get(format!(
                "{base}/pullrequests?searchCriteria.status={status}&api-version=7.1&$top=100&$skip={skip}"
            ))
            .await?;
            let batch = page
                .get("value")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let n = batch.len();
            prs.extend(batch);
            if n < 100 {
                break;
            }
            skip += n;
        }
    }
    let id_of = |pr: &serde_json::Value| {
        pr.get("pullRequestId")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    };
    prs.retain(|pr| {
        let id = id_of(pr);
        let date = text(pr, "closedDate");
        id != 0
            && window.min_pr_id.is_none_or(|lo| id >= lo)
            && window.max_pr_id.is_none_or(|hi| id <= hi)
            && window
                .completed_before
                .as_deref()
                .is_none_or(|cutoff| date.len() >= 10 && &date[..10] < cutoff)
    });
    prs.sort_by_key(|pr| std::cmp::Reverse(id_of(pr)));
    prs.dedup_by_key(|pr| id_of(pr));
    if let Some(cap) = window.max_prs {
        prs.truncate(cap);
    }

    let mut threads = Vec::new();
    for pr in &prs {
        let pr_id = id_of(pr);
        let pr_author = pr
            .get("createdBy")
            .map(|u| text(u, "displayName"))
            .unwrap_or_default();
        let pr_date = Some(text(pr, "closedDate"))
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| text(pr, "creationDate"));
        let body = match get(format!(
            "{base}/pullRequests/{pr_id}/threads?api-version=7.1"
        ))
        .await
        {
            Ok(body) => body,
            Err(e) => {
                tracing::warn!(pr = pr_id, "review threads fetch failed: {e}");
                continue;
            }
        };
        for thread in body
            .get("value")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
        {
            let comments: Vec<ReviewComment> = thread
                .get("comments")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter(|c| c.get("commentType").and_then(|v| v.as_str()) != Some("system"))
                .map(|c| ReviewComment {
                    author: c
                        .get("author")
                        .map(|a| text(a, "displayName"))
                        .unwrap_or_default(),
                    text: text(c, "content"),
                    likes: c
                        .get("usersLiked")
                        .and_then(|v| v.as_array())
                        .into_iter()
                        .flatten()
                        .map(|u| text(u, "displayName"))
                        .collect(),
                })
                .filter(|c| !c.text.trim().is_empty())
                .collect();
            if comments.is_empty() {
                continue;
            }
            let context = thread.get("threadContext");
            threads.push(ReviewThread {
                pr_id,
                pr_title: text(pr, "title"),
                pr_author: pr_author.clone(),
                pr_date: pr_date.clone(),
                thread_id: thread.get("id").and_then(|v| v.as_u64()).unwrap_or(0),
                status: text(thread, "status"),
                file_path: context.map(|c| text(c, "filePath")).unwrap_or_default(),
                line: context
                    .and_then(|c| c.get("rightFileStart"))
                    .and_then(|p| p.get("line"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32,
                comments,
            });
        }
    }
    Ok(threads)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dms() -> Vec<String> {
        vec!["Casey Lead".into(), "Casey Lead Agent".into()]
    }

    fn comment(author: &str, text: &str, likes: &[&str]) -> ReviewComment {
        ReviewComment {
            author: author.into(),
            text: text.into(),
            likes: likes.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn thread(pr_author: &str, status: &str, comments: Vec<ReviewComment>) -> ReviewThread {
        ReviewThread {
            pr_id: 7,
            pr_title: "Bulk export".into(),
            pr_author: pr_author.into(),
            pr_date: "2026-09-01T10:00:00Z".into(),
            thread_id: 1,
            status: status.into(),
            file_path: "/src/export.vb".into(),
            line: 12,
            comments,
        }
    }

    const ARGUMENT: &str = "Acknowledged, but I would rather leave this. The limit is a compile-time \
        constant, so changing it already means a code change, a build and a test pass, and the \
        resource strings are part of that same change set, so they cannot silently go stale.";

    #[test]
    fn replies_are_classified_by_what_they_decide() {
        let cases = [
            (
                "This is the intended behaviour.",
                Some(VerdictKind::Rejected),
            ),
            (
                "This is acceptable and per design.",
                Some(VerdictKind::Rejected),
            ),
            (
                "Out of scope of this PR. We need to look at the whole code base.",
                Some(VerdictKind::Deferred),
            ),
            (
                "Tracked in <a href=\"/x/_workitems/edit/1034\">User Story 1034</a>",
                Some(VerdictKind::Deferred),
            ),
            ("Fixed", Some(VerdictKind::Endorsed)),
            (
                "Confirmed valid and repaired; no need to change the caller.",
                Some(VerdictKind::Endorsed),
            ),
            (
                "Please address these findings.",
                Some(VerdictKind::Endorsed),
            ),
            ("Thanks, looking.", None),
        ];
        for (text, expected) in cases {
            assert_eq!(classify_reply(text), expected, "{text}");
        }
    }

    #[test]
    fn a_decision_makers_reply_outranks_the_thread_status() {
        // Closed as fixed, but the decision-maker rejected it.
        let t = thread(
            "Riley Dev",
            "fixed",
            vec![
                comment(
                    "Morgan Reviewer",
                    "Custom-report forms lack the per-row option.",
                    &[],
                ),
                comment(
                    "Casey Lead",
                    "This is the intended behaviour.",
                    &["Riley Dev"],
                ),
            ],
        );
        let v = thread_verdict(&t, &dms());
        assert_eq!(v.kind, VerdictKind::Rejected);
        assert_eq!(v.decided_by.as_deref(), Some("Casey Lead"));
        assert_eq!(
            v.reasoning.as_deref(),
            Some("This is the intended behaviour.")
        );
    }

    #[test]
    fn a_decision_makers_like_endorses_even_a_declined_finding() {
        let t = thread(
            "Riley Dev",
            "wontFix",
            vec![
                comment(
                    "Morgan Reviewer",
                    "The limit is hardcoded in every resx.",
                    &["Casey Lead"],
                ),
                comment("Riley Dev", ARGUMENT, &[]),
            ],
        );
        let v = thread_verdict(&t, &dms());
        assert_eq!(v.kind, VerdictKind::Endorsed);
        assert_eq!(v.basis, "decision_maker_like");
    }

    #[test]
    fn author_pushback_counts_only_with_an_argument() {
        let argued = thread(
            "Riley Dev",
            "wontFix",
            vec![
                comment("ReviewBot", "Use an enum.", &[]),
                comment("Riley Dev", ARGUMENT, &[]),
            ],
        );
        let v = thread_verdict(&argued, &dms());
        assert_eq!(v.kind, VerdictKind::ContestedByAuthor);
        assert!(v.reasoning.unwrap().starts_with("Acknowledged"));

        let bare = thread(
            "Riley Dev",
            "wontFix",
            vec![
                comment("ReviewBot", "Use an enum.", &[]),
                comment("Riley Dev", "Won't do.", &[]),
            ],
        );
        assert_eq!(
            thread_verdict(&bare, &dms()).kind,
            VerdictKind::DismissedWithoutReason
        );
    }

    #[test]
    fn a_decision_maker_authoring_the_pr_decides_their_own_argued_decline() {
        let t = thread(
            "Casey Lead",
            "wontFix",
            vec![
                comment("ReviewBot", "Handle the I/O failure.", &[]),
                comment("Casey Lead", ARGUMENT, &[]),
            ],
        );
        let v = thread_verdict(&t, &dms());
        assert_eq!(v.kind, VerdictKind::Rejected);
        assert_eq!(v.basis, "decision_maker_author");
    }

    #[test]
    fn decision_maker_findings_and_plain_statuses() {
        let raised = thread(
            "Riley Dev",
            "active",
            vec![comment("Casey Lead Agent", "Missing null check.", &[])],
        );
        assert_eq!(
            thread_verdict(&raised, &dms()).kind,
            VerdictKind::RaisedByDecisionMaker
        );
        let fixed = thread(
            "Riley Dev",
            "fixed",
            vec![comment("ReviewBot", "Typo.", &[])],
        );
        assert_eq!(thread_verdict(&fixed, &dms()).kind, VerdictKind::Fixed);
    }

    #[test]
    fn author_records_split_argued_declines_by_outcome() {
        let threads = vec![
            thread(
                "Riley Dev",
                "wontFix",
                vec![
                    comment("ReviewBot", "A", &[]),
                    comment("Riley Dev", ARGUMENT, &[]),
                ],
            ),
            thread(
                "Riley Dev",
                "wontFix",
                vec![
                    comment("ReviewBot", "B", &[]),
                    comment("Riley Dev", ARGUMENT, &[]),
                    comment("Casey Lead", "Please fix this.", &[]),
                ],
            ),
            thread("Riley Dev", "wontFix", vec![comment("ReviewBot", "C", &[])]),
            // Closed-as-outdated is not pushback.
            thread("Riley Dev", "closed", vec![comment("ReviewBot", "D", &[])]),
        ];
        let records = author_records(&threads, &dms());
        assert_eq!(
            records["Riley Dev"],
            AuthorRecord {
                argued_stood: 1,
                argued_overturned: 1,
                bare: 1
            }
        );
    }
}
