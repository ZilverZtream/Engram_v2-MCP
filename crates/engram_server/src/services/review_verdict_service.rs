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

/// Complete captured input is retained separately from its typed searchable
/// projection. JSONL preserves raw line bytes; providers preserve canonical
/// full supplied thread JSON before system/blank comment filtering.
pub struct CapturedReviewThread {
    pub thread: ReviewThread,
    pub raw_utf8: String,
    pub representation: String,
    pub provenance: serde_json::Value,
}

struct LimitedJson(Vec<u8>);
impl std::io::Write for LimitedJson {
    fn write(&mut self,bytes:&[u8])->std::io::Result<usize> {
        if self.0.len().checked_add(bytes.len()).is_none_or(|n|n>256*1024) {
            return Err(std::io::Error::other("provider thread exceeds 256 KiB; no import published"));
        }
        self.0.extend_from_slice(bytes);Ok(bytes.len())
    }
    fn flush(&mut self)->std::io::Result<()> {Ok(())}
}

async fn bounded_review_json(
    client:&reqwest::Client,url:&str,auth:&str,total:&mut usize,requests:&mut usize,deadline:tokio::time::Instant,
) -> anyhow::Result<serde_json::Value> {
    *requests=requests.checked_add(1).ok_or_else(||anyhow::anyhow!("provider request count overflow"))?;
    anyhow::ensure!(*requests<=1056,"review provider request budget exceeded; no import published");
    let mut response=tokio::time::timeout_at(deadline,client.get(url)
        .header(reqwest::header::AUTHORIZATION,auth)
        .header(reqwest::header::ACCEPT,"application/json").send()).await??.error_for_status()?;
    anyhow::ensure!(response.content_length().is_none_or(|n|n<=1024*1024),
        "review provider JSON body exceeds 1 MiB; no import published");
    let mut bytes=Vec::new();
    while let Some(chunk)=tokio::time::timeout_at(deadline,response.chunk()).await?? {
        let next=bytes.len().checked_add(chunk.len()).ok_or_else(||anyhow::anyhow!("provider body overflow"))?;
        let aggregate=total.checked_add(chunk.len()).ok_or_else(||anyhow::anyhow!("provider input overflow"))?;
        anyhow::ensure!(next<=1024*1024 && aggregate<=8*1024*1024,
            "review provider body/aggregate byte budget exceeded; no import published");
        *total=aggregate;
        bytes.extend_from_slice(&chunk);
    }
    Ok(serde_json::from_slice(&bytes)?)
}

fn provider_string<'a>(value:&'a serde_json::Value,key:&str)->anyhow::Result<&'a str> {
    let text=value.get(key).and_then(|v|v.as_str()).unwrap_or_default();
    anyhow::ensure!(text.len()<=4096,"provider identity field exceeds 4096 bytes");Ok(text)
}

/// Backward-compatible projection API. Managed ingestion uses the capture API
/// below so complete original comment evidence is not discarded.
pub async fn fetch_azure_devops_threads(
    org:&str,project:&str,repo:&str,pat:&str,window:&FetchWindow,
) -> anyhow::Result<Vec<ReviewThread>> {
    Ok(fetch_azure_devops_thread_captures(org,project,repo,pat,window).await?
        .into_iter().filter(|capture|!capture.thread.comments.is_empty()).map(|capture|capture.thread).collect())
}

pub async fn fetch_azure_devops_thread_captures(
    org:&str,project:&str,repo:&str,pat:&str,window:&FetchWindow,
) -> anyhow::Result<Vec<CapturedReviewThread>> {
    anyhow::ensure!(org.len()<=1024 && project.len()<=1024 && repo.len()<=1024
        && window.max_prs.is_none_or(|n|n<=1024),"review provider coordinate/PR limit exceeded");
    let auth=format!("Basic {}",crate::services::code_review_ingest_service::base64_encode(format!(":{pat}").as_bytes()));
    let client=reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)).build()?;
    let esc=crate::services::code_review_ingest_service::url_escape;
    let base=format!("https://dev.azure.com/{}/{}/_apis/git/repositories/{}",esc(org),esc(project),esc(repo));
    anyhow::ensure!(base.len()<=4096,"review provider URL exceeds source locator budget");
    fetch_review_captures_at(&client,&base,&auth,window).await
}

async fn fetch_review_captures_at(client:&reqwest::Client,base:&str,auth:&str,window:&FetchWindow)
    ->anyhow::Result<Vec<CapturedReviewThread>> {
    let deadline=tokio::time::Instant::now()+std::time::Duration::from_secs(120);
    let (mut total,mut requests,mut pages)=(0usize,0usize,0usize);
    let mut prs=std::collections::BTreeMap::new();
    for status in ["completed","active"] {
        let mut skip=0usize;
        loop {
            pages+=1;anyhow::ensure!(pages<=32,"review provider PR page budget exceeded; no import published");
            let page=bounded_review_json(&client,&format!("{base}/pullrequests?searchCriteria.status={status}&api-version=7.1&$top=100&$skip={skip}"),
                &auth,&mut total,&mut requests,deadline).await?;
            let batch=page.get("value").and_then(|v|v.as_array())
                .ok_or_else(||anyhow::anyhow!("provider PR page lacks its value array"))?;
            anyhow::ensure!(batch.len()<=100,"unsupported provider PR page size");
            for pr in batch {
                let id=pr.get("pullRequestId").and_then(|v|v.as_u64()).unwrap_or(0);
                let closed=provider_string(pr,"closedDate")?;
                if id==0 || window.min_pr_id.is_some_and(|lo|id<lo)
                    || window.max_pr_id.is_some_and(|hi|id>hi)
                    || window.completed_before.as_deref().is_some_and(|cut|
                        closed.get(..10).is_none_or(|date|date>=cut)) {continue;}
                if prs.contains_key(&id) {continue;}
                anyhow::ensure!(prs.len()<1024,"review provider exceeds 1024 selected PRs; no import published");
                let author=pr.get("createdBy").map(|v|provider_string(v,"displayName")).transpose()?.unwrap_or_default();
                let date=if closed.is_empty(){provider_string(pr,"creationDate")?}else{closed};
                prs.insert(id,(provider_string(pr,"title")?.to_owned(),author.to_owned(),date.to_owned()));
            }
            if batch.len()<100 {break;}
            skip=skip.checked_add(batch.len()).ok_or_else(||anyhow::anyhow!("provider PR offset overflow"))?;
        }
    }
    let mut captured=Vec::new();
    let mut capture_bytes=0usize;
    for (pr_id,(title,author,date)) in prs.into_iter().rev().take(window.max_prs.unwrap_or(1024)) {
        let url=format!("{base}/pullRequests/{pr_id}/threads?api-version=7.1");
        // A failed PR no longer silently becomes a successful partial import.
        let body=bounded_review_json(&client,&url,&auth,&mut total,&mut requests,deadline).await?;
        let values=body.get("value").and_then(|v|v.as_array())
            .ok_or_else(||anyhow::anyhow!("provider thread response lacks its value array"))?;
        anyhow::ensure!(values.len()<=1024,"provider exceeds 1024 supplied threads");
        for original in values {
            anyhow::ensure!(captured.len()<1024,"review provider exceeds 1024 supplied threads; no import published");
            let comments=original.get("comments").and_then(|v|v.as_array())
                .ok_or_else(||anyhow::anyhow!("provider thread lacks comments array"))?;
            anyhow::ensure!(comments.len()<=1024,"provider thread exceeds 1024 comments");
            let mut encoded=LimitedJson(Vec::new());
            serde_json::to_writer(&mut encoded,original)?;
            let raw_utf8=String::from_utf8(encoded.0)?;
            capture_bytes=capture_bytes.checked_add(raw_utf8.len()).ok_or_else(||anyhow::anyhow!("provider capture overflow"))?;
            anyhow::ensure!(capture_bytes<=4*1024*1024,"provider capture batch exceeds 4 MiB; no import published");
            let mut projected=Vec::new();
            for comment in comments {
                let likes=comment.get("usersLiked").and_then(|v|v.as_array());
                anyhow::ensure!(likes.is_none_or(|v|v.len()<=256),"provider comment exceeds 256 likes");
                if comment.get("commentType").and_then(|v|v.as_str())==Some("system") {continue;}
                let text=comment.get("content").and_then(|v|v.as_str()).unwrap_or_default();
                if text.trim().is_empty() {continue;}
                projected.push(ReviewComment {author:comment.get("author")
                    .map(|v|provider_string(v,"displayName")).transpose()?.unwrap_or_default().to_owned(),
                    text:text.to_owned(),likes:likes.into_iter().flatten()
                        .map(|v|provider_string(v,"displayName").map(str::to_owned)).collect::<anyhow::Result<_>>()?});
            }
            let thread_id=original.get("id").and_then(|v|v.as_u64()).unwrap_or(0);
            let context=original.get("threadContext");
            let line=context.and_then(|v|v.get("rightFileStart")).and_then(|v|v.get("line")).and_then(|v|v.as_u64()).unwrap_or(0);
            let thread=ReviewThread {pr_id,pr_title:title.clone(),pr_author:author.clone(),pr_date:date.clone(),thread_id,
                status:provider_string(original,"status")?.to_owned(),
                file_path:context.map(|v|provider_string(v,"filePath")).transpose()?.unwrap_or_default().to_owned(),
                line:u32::try_from(line)?,comments:projected};
            // Preserve blank/system-only threads too. Their projection is Open;
            // the complete supplied original remains recoverable without loss.
            captured.push(CapturedReviewThread {thread,raw_utf8,representation:"canonical_supplied_provider_thread_json".into(),
                provenance:serde_json::json!({"source_kind":"azure_devops","source_url":url,
                    "pr_id":pr_id,"pr_title":title,"pr_author":author,"pr_date":date,
                    "scope":"Complete supplied thread JSON before system/blank-comment projection. Canonical JSON, not original HTTP wire bytes; unreturned provider history, identity authenticity and source-date completeness are not certified. Authentication headers and tokens are not retained."})});
        }
    }
    Ok(captured)
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


#[cfg(test)]
mod capture_tests {
    use super::*;
    use std::{io::{Read,Write},net::TcpListener,sync::{Arc,atomic::{AtomicBool,AtomicUsize,Ordering}},thread,time::Duration};
    struct OwnedProvider {stop:Arc<AtomicBool>,oversized:Arc<AtomicBool>,calls:Arc<AtomicUsize>,worker:Option<thread::JoinHandle<()>>,url:String}
    impl Drop for OwnedProvider {
        fn drop(&mut self){self.stop.store(true,Ordering::SeqCst);if let Some(worker)=self.worker.take(){let _=worker.join();}}
    }
    #[test]
    fn actual_provider_capture_preserves_system_blank_unknown_fields_and_refuses_oversized_body_before_decode()->anyhow::Result<()> {
        let original=serde_json::json!({"id":7,"status":"fixed","unknown_thread_field":{"version":9},
            "threadContext":{"filePath":"/src/export.rs","rightFileStart":{"line":4},"unknown_context":"keep"},
            "comments":[{"id":1,"commentType":"text","author":{"displayName":"Reviewer","id":"raw-author-id"},
                "content":"Qualified finding","publishedDate":"2026-01-01T12:00:00Z","unknown_field":"keep"},
                {"id":2,"commentType":"system","author":{"displayName":"System"},"content":"original system record"},
                {"id":3,"commentType":"text","author":{"displayName":"Blank author"},"content":"","unknown_blank":"keep"}]});
        let listener=TcpListener::bind("127.0.0.1:0")?;listener.set_nonblocking(true)?;
        let address=listener.local_addr()?;
        let stop=Arc::new(AtomicBool::new(false));let oversized=Arc::new(AtomicBool::new(false));let calls=Arc::new(AtomicUsize::new(0));
        let (st,ov,cs)=(stop.clone(),oversized.clone(),calls.clone());let thread_value=original.clone();
        let worker=thread::spawn(move||{
            while !st.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut socket,_))=>{
                        let result=(||->anyhow::Result<()> {
                            // Windows accepts can inherit the listener's nonblocking mode.
                            // Switch before bounded blocking reads; readiness is not a request boundary.
                            socket.set_nonblocking(false)?;
                            socket.set_read_timeout(Some(Duration::from_secs(2)))?;socket.set_write_timeout(Some(Duration::from_secs(2)))?;
                            let mut request=Vec::new();let mut buffer=[0u8;1024];
                            while !request.windows(4).any(|v|v==b"\r\n\r\n") {
                                let n=socket.read(&mut buffer)?;anyhow::ensure!(n>0 && request.len()+n<=8192);request.extend_from_slice(&buffer[..n]);
                            }
                            cs.fetch_add(1,Ordering::SeqCst);
                            if ov.load(Ordering::SeqCst) {
                                write!(socket,"HTTP/1.1 200 OK\r\nContent-Length: 1048577\r\nConnection: close\r\n\r\n")?;
                            } else {
                                let request=std::str::from_utf8(&request)?;
                                let body=(if request.contains("/pullRequests/100/threads?") {
                                    serde_json::json!({"value":[thread_value]})
                                } else if request.contains("searchCriteria.status=completed") {
                                    serde_json::json!({"value":[{"pullRequestId":100,"title":"Qualified discussion","createdBy":{"displayName":"Author"},"closedDate":"2026-01-01T12:00:00Z"}]})
                                } else {serde_json::json!({"value":[]})}).to_string();
                                write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len())?;
                            }
                            socket.flush()?;Ok(())
                        })();let _=result;
                    }
                    Err(error) if error.kind()==std::io::ErrorKind::WouldBlock=>thread::sleep(Duration::from_millis(5)),
                    Err(_)=>break,
                }
            }
        });
        let provider=OwnedProvider {stop,oversized,calls,worker:Some(worker),url:format!("http://{address}")};
        let runtime=tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
        let result:anyhow::Result<()>=runtime.block_on(async {
            let client=reqwest::Client::builder().timeout(Duration::from_secs(3)).build()?;
            let captured=fetch_review_captures_at(&client,&provider.url,"Basic owned-test-token",&FetchWindow::default()).await?;
            anyhow::ensure!(captured.len()==1 && captured[0].thread.comments.len()==1);
            anyhow::ensure!(serde_json::from_str::<serde_json::Value>(&captured[0].raw_utf8)?==original);
            anyhow::ensure!(captured[0].thread.comments[0].text=="Qualified finding");
            anyhow::ensure!(!captured[0].provenance.to_string().contains("owned-test-token"));
            anyhow::ensure!(provider.calls.load(Ordering::SeqCst)==3);
            provider.oversized.store(true,Ordering::SeqCst);
            let error=fetch_review_captures_at(&client,&provider.url,"Basic owned-test-token",&FetchWindow::default()).await.err()
                .ok_or_else(||anyhow::anyhow!("oversized declared provider body must be refused"))?;
            anyhow::ensure!(error.to_string().contains("body exceeds 1 MiB"));
            anyhow::ensure!(provider.calls.load(Ordering::SeqCst)==4);
            Ok(())
        });
        drop(runtime);drop(provider);result
    }
}
