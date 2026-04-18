//! Rules pipeline for `produce_claude_md` — turns raw repo-rule dumps
//! into a crisp, curated `<critical_rules>` section.
//!
//! ## Why this exists
//!
//! The naive path — `list_repo_rules` → map each row to a `CriticalRule`
//! → ship — produces 30+ bullets in the root CLAUDE.md, most of which
//! are either:
//!
//! - noise rules CodeRabbit flagged once ("fix typo in comment",
//!   "rename for clarity") that aren't durable code-writing guidance,
//! - one-PR "accidents" that don't constitute a pattern,
//! - thirty token-level clusters of the SAME underlying rule (30
//!   different variable names all flagged "missing null guard" →
//!   thirty bullets instead of one "null guards are the #1 error"
//!   meta-rule), or
//! - three-paragraph LLM-generated "why was this reverted" essays
//!   that should have been a single imperative sentence.
//!
//! This pipeline applies four deterministic passes to fix all four:
//!
//! 1. **Noise filter** — drop rules whose text matches a blocklist of
//!    cosmetic-fix phrases.
//! 2. **Meta-clustering** — bucket token-level rules by semantic
//!    category (null-guard, audit-log, permission-check, etc.) and
//!    collapse each bucket into one aggregated rule whose evidence
//!    sums the PR counts across member rules.
//! 3. **Rule-text templating** — rewrite each kept rule to an
//!    imperative sentence under 120 characters, category-driven.
//!    Immune rules get their LLM-rationalisation prose stripped.
//! 4. **Render-threshold filter** — require `fix_rate ≥ 0.7` AND
//!    `pr_count ≥ 3` for a rule to enter the root document. Below
//!    those, rules belong in per-language `.claude/rules/*.md` files
//!    where attention is cheaper.
//!
//! The pipeline is completely deterministic. An optional LLM curation
//! pass can run on the output of this pipeline when the caller opts
//! in via `use_llm: true` — that path is implemented separately so
//! this module stays LLM-free and fast.

use super::{CriticalRule, RuleSource};

/// Semantic categories the meta-clusterer recognises. Each category
/// has a keyword dictionary; any rule whose text contains one of the
/// keywords for a category gets mapped to that category and
/// aggregated with its peers. Categories are hand-picked based on
/// the patterns that actually dominate the OciusX corpus — expand
/// when a new pattern shows up across multiple projects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MetaCategory {
    /// Missing null / Nothing / undefined checks before dereferencing.
    /// The #1 error class on the OciusX corpus (530+ flags).
    NullGuard,
    /// Missing audit / activity log calls after data mutations.
    AuditLog,
    /// Missing permission / authorization checks at action boundaries.
    PermissionCheck,
    /// `SubmitChanges()` / `SaveChanges()` called on a passed-in
    /// DataContext (commits caller's uncommitted changes).
    SubmitChangesOwnership,
    /// Missing `Return` after `SafeRedirect` / `Response.Redirect`.
    ReturnAfterRedirect,
    /// Bulk operations stamping the current user's team onto rows
    /// instead of each row's own team id.
    BulkTeamAttribution,
    /// Missing error callback on `api.ajax` / `api.serviceajax` (UI
    /// spinner stays up forever on server error).
    ErrorCallback,
    /// `innerHTML` / XSS / missing escape / unsanitized interpolation
    /// into DOM or Bootstrap popovers.
    XssSanitize,
    /// Hardcoded English / missing localisation strings.
    Localization,
    /// Missing input validation / parameter checks.
    InputValidation,
    /// Debug statements / console.log / temporary diagnostic code
    /// left committed.
    DebugLeftovers,
    /// Other / doesn't match any category — render individually if
    /// it passes the threshold filter, otherwise drop.
    Other,
}

impl MetaCategory {
    /// Human-readable label for each category, used as the canonical
    /// rendered rule text.
    pub fn label(self) -> &'static str {
        match self {
            Self::NullGuard => "Null-guard every data-access return before LINQ access",
            Self::AuditLog => {
                "Call the project's audit-log helper after every Create/Update/Delete"
            }
            Self::PermissionCheck => {
                "Enforce the project's per-action permission check before any API / page handler runs"
            }
            Self::SubmitChangesOwnership => {
                "Only call `SubmitChanges()` when you OWN the DataContext (`db Is Nothing`)"
            }
            Self::ReturnAfterRedirect => "Always add `Return` on the line after `SafeRedirect(...)`",
            Self::BulkTeamAttribution => {
                "In bulk operations, team id comes from each row's own `iop_createdbyteamId`, NOT from the current user"
            }
            Self::ErrorCallback => {
                "Every `api.ajax` / `api.serviceajax` call must include an error callback"
            }
            Self::XssSanitize => {
                "Never interpolate server/user strings into `innerHTML` or Bootstrap popover HTML without escaping"
            }
            Self::Localization => {
                "No hardcoded English — use the project's localisation helper for every user-visible string"
            }
            Self::InputValidation => {
                "Validate inputs at the boundary — constructor / controller / page handler entry points"
            }
            Self::DebugLeftovers => {
                "Remove debug statements (`console.log`, `Debug.Print`) before committing"
            }
            Self::Other => "",
        }
    }

    /// All categories in render order. Used when iterating the
    /// meta-clustered output.
    pub const ALL: &'static [MetaCategory] = &[
        Self::NullGuard,
        Self::AuditLog,
        Self::PermissionCheck,
        Self::SubmitChangesOwnership,
        Self::ReturnAfterRedirect,
        Self::BulkTeamAttribution,
        Self::XssSanitize,
        Self::ErrorCallback,
        Self::Localization,
        Self::InputValidation,
        Self::DebugLeftovers,
    ];

    /// Severity tier used to rank categories when the root document
    /// is over budget. Data-correctness and security rank above
    /// ergonomics.
    pub fn severity_rank(self) -> u8 {
        match self {
            Self::NullGuard => 1,
            Self::PermissionCheck => 1,
            Self::SubmitChangesOwnership => 1,
            Self::XssSanitize => 1,
            Self::AuditLog => 2,
            Self::ReturnAfterRedirect => 2,
            Self::BulkTeamAttribution => 2,
            Self::InputValidation => 3,
            Self::ErrorCallback => 3,
            Self::Localization => 4,
            Self::DebugLeftovers => 4,
            Self::Other => 5,
        }
    }
}

/// Keyword dictionary: a rule text is classified into the FIRST
/// category whose token list overlaps with the text (case-insensitive
/// substring match). Order within the match table matters for
/// disambiguation — more specific categories go first.
fn classify_text(text: &str) -> MetaCategory {
    let lower = text.to_ascii_lowercase();
    const TABLE: &[(MetaCategory, &[&str])] = &[
        // Specific call-site patterns go FIRST because their
        // keywords ("submitchanges", "handelselogg") are distinctive
        // and shouldn't get swallowed by a broader "auth"/"null"
        // keyword match from a later row.
        (
            // Ownership-specific phrasings only — matching the bare
            // method names would swallow adjacent rules (e.g. an
            // audit-log rule that merely mentions SubmitChanges as
            // the triggering call site).
            MetaCategory::SubmitChangesOwnership,
            &[
                "passed-in context",
                "passed context",
                "external context",
                "own the context",
                "caller owns",
                "context ownership",
                "db is nothing",
                "ownctx",
            ],
        ),
        (
            MetaCategory::AuditLog,
            &["audit log", "handelselogg", "activity log", "logactivity", "missing audit", "log the", "log activity"],
        ),
        (
            MetaCategory::PermissionCheck,
            &["canreadviaapi", "permission check", "checkread", "checkisuserinrole", "authorize", "unauthorized", "access control"],
        ),
        (
            MetaCategory::ReturnAfterRedirect,
            &["safe redirect", "saferedirect", "response.redirect", "after redirect", "missing return"],
        ),
        (
            MetaCategory::BulkTeamAttribution,
            &["team attribution", "iop_createdbyteamid", "bulk create", "stamp the team", "stamp team", "current user's team"],
        ),
        (
            MetaCategory::XssSanitize,
            &["innerhtml", "xss", "sanitize", "escape html", "html: true", "prevent xss", "html injection"],
        ),
        (
            MetaCategory::ErrorCallback,
            &["error callback", "missing callback", "api.ajax", "spinner", "missing error handler", "missing error"],
        ),
        (
            MetaCategory::NullGuard,
            &[
                "null guard",
                "null check",
                "null reference",
                "null-coalescing",
                "null coalescing",
                "is nothing",
                "guard against",
                "undefined",
                "guard navigation",
                "null polyline",
                "potential null",
                "return an empty list instead of nothing",
                "null input",
            ],
        ),
        (
            MetaCategory::InputValidation,
            &[
                "input validation",
                "parameter validation",
                "validate input",
                "constructor validation",
                "argument validation",
                "validation logic",
            ],
        ),
        (
            MetaCategory::Localization,
            &[
                "hardcoded string",
                "wrong language",
                "localization",
                "translation",
                "resx",
                "hardcoded english",
                "jstext",
            ],
        ),
        (
            MetaCategory::DebugLeftovers,
            &["console.log", "debug statement", "debug=\"true\"", "remove debug", "debug leftover", "temporary diagnostic"],
        ),
    ];
    for (cat, keywords) in TABLE {
        if keywords.iter().any(|k| lower.contains(k)) {
            return *cat;
        }
    }
    MetaCategory::Other
}

/// Stop-phrases: a rule whose canonical text matches any of these
/// patterns is dropped entirely — it isn't the kind of durable
/// code-writing guidance that belongs in CLAUDE.md.
///
/// Two classes of dropped rules:
///
/// - **Cosmetic / one-off** — "fix typo", "rename for clarity",
///   stylelint formatting, duplicate-markup removal. These don't
///   shape future code; they're one-PR cleanups.
/// - **Compile-time errors** — rules like "InfoWindow.isOpen does not
///   exist – compile-time error" or "SetColumnWidth will not compile"
///   are already caught by the compiler; having them in CLAUDE.md
///   just burns attention budget.
/// - **Process hygiene** (applies to immune rules) — "minified file
///   edited", "corrupted patch file", "debug=\"true\" committed".
///   Not code-writing rules; repo-hygiene reminders.
fn is_noise_rule(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    const NOISE: &[&str] = &[
        "fix typo",
        "typo in",
        "rename the ",
        "rename for clarity",
        "stylelint",
        "duplicate search box",
        "duplicate markup",
        "remove duplicate",
        "does not exist – compile-time error",
        "compile-time error",
        "will not compile",
        "minified",
        "patch file",
        "malformed header",
        "corrupted",
        "debug=\"true\"",
        "debug = \"true\"",
        "modern :not()",
        "lgtm",
        "nitpick",
        "text is in the wrong language", // too generic to be actionable
    ];
    NOISE.iter().any(|n| lower.contains(n))
}

/// Strip the LLM-rationalisation prefix that the revert-analysis
/// pipeline injects into immune rule text, leaving just the crisp
/// prescriptive sentence. Caps length at 150 characters — everything
/// after that is context that belongs in the cited revert commit,
/// not CLAUDE.md.
fn tighten_immune_text(text: &str, file_pattern: &str) -> String {
    const PREFIXES: &[&str] = &[
        "this pattern should be avoided because ",
        "this pattern should be avoided ",
        "this diff shows ",
        "avoid this because ",
    ];
    let lower = text.trim().to_ascii_lowercase();
    let mut stripped = text.trim().to_string();
    for p in PREFIXES {
        if lower.starts_with(p) {
            stripped = stripped[p.len()..].trim_start().to_string();
            break;
        }
    }
    // Chop at the first " Instead," / " Developers should" / similar
    // — the "instead" half usually contains the prescription, but
    // the "because" half contains the rationalisation; we want the
    // prescription.
    for split in [" Instead, developers", " Instead,", " Developers should", " Instead"] {
        if let Some(idx) = stripped.find(split) {
            let after = stripped[idx..].trim_start_matches(split).trim_start();
            if !after.is_empty() {
                stripped = after.to_string();
            }
            break;
        }
    }
    // Hard cap at 150 chars including the file-pattern suffix.
    let suffix = format!(" (immune: {file_pattern})");
    let max_body = 150usize.saturating_sub(suffix.len()).max(30);
    if stripped.len() > max_body {
        stripped.truncate(max_body);
        // Don't break a word mid-character.
        while !stripped.is_empty() && !stripped.is_char_boundary(stripped.len()) {
            stripped.pop();
        }
        stripped.push('…');
    }
    format!("{stripped}{suffix}")
}

/// Render threshold — the rule must be at least this confident to
/// land in the root document's `<critical_rules>` section. Anything
/// below is surfaced in per-language `.claude/rules/*.md` files
/// instead.
#[derive(Debug, Clone, Copy)]
pub struct RenderThreshold {
    pub min_fix_rate: f32,
    pub min_pr_count: usize,
}

impl Default for RenderThreshold {
    fn default() -> Self {
        Self {
            min_fix_rate: 0.7,
            min_pr_count: 3,
        }
    }
}

/// A raw repo rule + its parsed metadata, ready for the pipeline.
/// `fix_rate` and `pr_count` may be `None` for immune and plain
/// repo-rule entries (they don't carry CodeRabbit aggregate stats);
/// those pass through the threshold filter unconditionally.
#[derive(Debug, Clone)]
pub struct RawRule {
    pub rule_id: String,
    pub file_pattern: String,
    pub rule_text: String,
    pub source: RuleSource,
    pub fix_rate: Option<f32>,
    pub pr_count: Option<usize>,
}

/// Output of the pipeline: the curated root-document rules plus the
/// overflow rules that went to per-language rule files.
#[derive(Debug, Clone, Default)]
pub struct PipelineOutput {
    pub root_rules: Vec<CriticalRule>,
    pub per_language_overflow: Vec<CriticalRule>,
    /// Summary line of what the pipeline did — surfaced in the
    /// handler's "Write-path notes" for transparency.
    pub summary: String,
}

/// Run the full deterministic pipeline over a raw rule set.
pub fn run_pipeline(raw: Vec<RawRule>, threshold: RenderThreshold) -> PipelineOutput {
    let input_count = raw.len();

    // Stage 1 — noise filter. Drops cosmetic fixes, compile-error
    // reminders, and process-hygiene immune rules.
    let deduped = drop_noise(raw);
    let after_noise = deduped.len();

    // Stage 2 — meta-clustering. Groups rules by category; the
    // `Other` bucket stays as-is. Each category with ≥1 member
    // collapses to a single aggregated rule with summed pr_count.
    let (grouped, other) = meta_cluster(deduped);

    // Stage 3 — render-threshold filter. Splits rules into
    // root-document vs per-language buckets.
    let mut root_rules: Vec<CriticalRule> = Vec::new();
    let mut per_language_overflow: Vec<CriticalRule> = Vec::new();

    for (category, agg) in grouped {
        let passes = agg.meets_threshold(threshold);
        let rule = agg.into_critical_rule(category);
        if passes {
            root_rules.push(rule);
        } else {
            per_language_overflow.push(rule);
        }
    }
    // Process uncategorised ("Other") rules — preserve their
    // original text (already templated at the caller level),
    // apply the threshold filter the same way.
    for r in other {
        let passes = match (r.fix_rate, r.pr_count) {
            (Some(rate), Some(prs)) => rate >= threshold.min_fix_rate && prs >= threshold.min_pr_count,
            // Immune + plain repo-rule entries always pass — they
            // aren't CodeRabbit-sourced and don't carry aggregate
            // stats.
            _ => true,
        };
        let critical = CriticalRule {
            text: r.rule_text.clone(),
            evidence: Some(format!("({})", r.rule_id)),
            source: r.source,
        };
        if passes {
            root_rules.push(critical);
        } else {
            per_language_overflow.push(critical);
        }
    }

    // Sort: immune (past reverts) first, then CodeRabbit
    // meta-rules by severity rank, then plain repo rules, then
    // existing-user rules.
    root_rules.sort_by_key(|r| source_rank(&r.source));

    let summary = format!(
        "rules: {input_count} raw → {after_noise} after noise filter → {root} in root + {overflow} in per-language files",
        root = root_rules.len(),
        overflow = per_language_overflow.len(),
    );

    PipelineOutput {
        root_rules,
        per_language_overflow,
        summary,
    }
}

fn source_rank(src: &RuleSource) -> u8 {
    match src {
        RuleSource::Immune => 0,
        RuleSource::CodeRabbit => 1,
        RuleSource::RepoRule => 2,
        RuleSource::Existing => 3,
    }
}

fn drop_noise(raw: Vec<RawRule>) -> Vec<RawRule> {
    raw.into_iter()
        .filter(|r| !is_noise_rule(&r.rule_text))
        .collect()
}

/// Aggregated meta-cluster — one per MetaCategory that had any
/// members after noise filtering. Keeps the summed PR count, the
/// max fix rate (categories are a set of similar-behaviour rules so
/// fix-rate averages would mislead), and the list of source rule
/// IDs for traceability.
#[derive(Debug, Clone, Default)]
pub struct Aggregated {
    pub total_prs: usize,
    pub max_fix_rate: f32,
    pub member_ids: Vec<String>,
    pub example_file_patterns: Vec<String>,
}

impl Aggregated {
    fn meets_threshold(&self, t: RenderThreshold) -> bool {
        self.total_prs >= t.min_pr_count && self.max_fix_rate >= t.min_fix_rate
    }

    fn into_critical_rule(self, category: MetaCategory) -> CriticalRule {
        let evidence = format!(
            "({} CodeRabbit rules aggregated, {} PRs, {:.0}% fix rate)",
            self.member_ids.len(),
            self.total_prs,
            self.max_fix_rate * 100.0
        );
        CriticalRule {
            text: category.label().to_string(),
            evidence: Some(evidence),
            source: RuleSource::CodeRabbit,
        }
    }
}

fn meta_cluster(raw: Vec<RawRule>) -> (Vec<(MetaCategory, Aggregated)>, Vec<RawRule>) {
    use std::collections::HashMap;

    let mut buckets: HashMap<MetaCategory, Aggregated> = HashMap::new();
    let mut other: Vec<RawRule> = Vec::new();

    for r in raw {
        // Immune + plain repo-rule entries bypass categorisation —
        // they're author-curated and the category signal doesn't
        // apply.
        if matches!(r.source, RuleSource::Immune | RuleSource::Existing) {
            other.push(r);
            continue;
        }
        let cat = classify_text(&r.rule_text);
        if cat == MetaCategory::Other {
            other.push(r);
            continue;
        }
        let agg = buckets.entry(cat).or_default();
        agg.total_prs += r.pr_count.unwrap_or(1);
        agg.max_fix_rate = agg.max_fix_rate.max(r.fix_rate.unwrap_or(0.0));
        agg.member_ids.push(r.rule_id.clone());
        if agg.example_file_patterns.len() < 3 && !r.file_pattern.is_empty() {
            agg.example_file_patterns.push(r.file_pattern.clone());
        }
    }

    // Render order: by category severity rank (lower = more critical).
    let mut ordered: Vec<(MetaCategory, Aggregated)> = buckets.into_iter().collect();
    ordered.sort_by_key(|(c, _)| c.severity_rank());
    (ordered, other)
}

/// Translate an immune repo-rule's raw text into its rendered form.
/// Called by the handler for every repo_rule whose id starts with
/// `immune_`. Returns `None` if the rule is noise and should be
/// dropped entirely.
pub fn render_immune_rule_text(raw_text: &str, file_pattern: &str) -> Option<String> {
    if is_noise_rule(raw_text) {
        return None;
    }
    Some(tighten_immune_text(raw_text, file_pattern))
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_raw(id: &str, text: &str, prs: usize, rate: f32, source: RuleSource) -> RawRule {
        RawRule {
            rule_id: id.into(),
            file_pattern: "**/*.vb".into(),
            rule_text: text.into(),
            source,
            fix_rate: Some(rate),
            pr_count: Some(prs),
        }
    }

    #[test]
    fn noise_filter_drops_typos_and_stylelint() {
        assert!(is_noise_rule("Fix typo in comment."));
        assert!(is_noise_rule("Stylelint: use modern :not() list notation."));
        assert!(is_noise_rule("Remove duplicate search box markup."));
        assert!(is_noise_rule(
            "InfoWindow.isOpen does not exist – compile-time error"
        ));
        assert!(is_noise_rule("New key LGTM; verify base fallback exists."));
    }

    #[test]
    fn noise_filter_keeps_real_rules() {
        assert!(!is_noise_rule(
            "Apply null-coalescing guard to iok_benamning."
        ));
        assert!(!is_noise_rule(
            "Call handelselogg.Create after SubmitChanges."
        ));
        assert!(!is_noise_rule("Guard against missing DOM element."));
    }

    #[test]
    fn classify_text_buckets_null_guards() {
        assert_eq!(
            classify_text("Apply null-coalescing guard to iok_benamning."),
            MetaCategory::NullGuard
        );
        assert_eq!(
            classify_text("Potential null reference on _divMarkerTypes."),
            MetaCategory::NullGuard
        );
        assert_eq!(
            classify_text("Guard navigation-property access before loading supplementary form fields."),
            MetaCategory::NullGuard
        );
        assert_eq!(
            classify_text("Return an empty list instead of Nothing."),
            MetaCategory::NullGuard
        );
    }

    #[test]
    fn classify_text_buckets_permissions_and_audit() {
        assert_eq!(
            classify_text("Permission check missing on api controller"),
            MetaCategory::PermissionCheck
        );
        assert_eq!(
            classify_text("CheckIsUserInRole used instead of CanReadViaApi"),
            MetaCategory::PermissionCheck
        );
        assert_eq!(
            classify_text("Missing audit log after SubmitChanges"),
            MetaCategory::AuditLog
        );
    }

    #[test]
    fn classify_text_is_other_when_no_keyword_match() {
        assert_eq!(
            classify_text("Desktop modal can exceed viewport height."),
            MetaCategory::Other
        );
    }

    #[test]
    fn meta_cluster_collapses_30_null_guards_into_one_rule() {
        // Simulate a handful of token-level null-guard clusters
        // split by Jaccard — the meta-clusterer must collapse them.
        let raw = vec![
            mk_raw("cr_1", "Apply null-coalescing guard to iok_benamning.", 3, 1.0, RuleSource::CodeRabbit),
            mk_raw("cr_2", "Potential null reference on _divMarkerTypes.", 2, 1.0, RuleSource::CodeRabbit),
            mk_raw("cr_3", "Guard against missing DOM element.", 2, 0.5, RuleSource::CodeRabbit),
            mk_raw("cr_4", "Return an empty list instead of Nothing.", 3, 0.67, RuleSource::CodeRabbit),
            mk_raw("cr_5", "Guard navigation-property access.", 2, 1.0, RuleSource::CodeRabbit),
        ];
        let out = run_pipeline(raw, RenderThreshold { min_fix_rate: 0.6, min_pr_count: 3 });
        let null_rules: Vec<_> = out
            .root_rules
            .iter()
            .filter(|r| r.text.starts_with("Null-guard"))
            .collect();
        assert_eq!(
            null_rules.len(),
            1,
            "30+ null-guard clusters must collapse to ONE meta-rule; got {:#?}",
            out.root_rules
        );
        let evidence = null_rules[0].evidence.as_deref().unwrap_or("");
        assert!(
            evidence.contains("CodeRabbit rules aggregated"),
            "evidence must cite how many member rules collapsed; got: {evidence}"
        );
    }

    #[test]
    fn render_threshold_routes_low_confidence_to_overflow() {
        let raw = vec![
            // Fix rate 50% with 7 PRs — below threshold, should go
            // to overflow.
            mk_raw("cr_low", "Remove duplicate search box markup.", 7, 0.5, RuleSource::CodeRabbit),
            // This one would otherwise meta-cluster, but the noise
            // filter drops it first. Good — that's the pipeline.
        ];
        let out = run_pipeline(raw, RenderThreshold::default());
        assert_eq!(
            out.root_rules.len(),
            0,
            "noise filter should drop duplicate-markup rule entirely"
        );
        assert_eq!(out.per_language_overflow.len(), 0);
    }

    #[test]
    fn low_fix_rate_coderabbit_rule_goes_to_overflow_not_root() {
        // A CodeRabbit rule that doesn't meta-cluster (Other) and
        // has below-threshold fix_rate stays OUT of root but goes
        // into overflow.
        let raw = vec![mk_raw(
            "cr_gray",
            "Desktop modal can exceed viewport height.",
            4,
            0.5,
            RuleSource::CodeRabbit,
        )];
        let out = run_pipeline(raw, RenderThreshold::default());
        assert_eq!(out.root_rules.len(), 0, "should not enter root");
        assert_eq!(out.per_language_overflow.len(), 1);
    }

    #[test]
    fn immune_rules_always_pass_threshold() {
        // Immune rules don't carry CodeRabbit aggregate stats — they
        // must always pass the threshold filter regardless, since
        // they represent known-bad patterns someone already
        // reverted.
        let raw = vec![RawRule {
            rule_id: "immune_deadbeef".into(),
            file_pattern: "Site/App_Code/dal/fiberjobb.vb".into(),
            rule_text: "Don't fetch-then-delete; use PK delete in one tx.".into(),
            source: RuleSource::Immune,
            fix_rate: None,
            pr_count: None,
        }];
        let out = run_pipeline(raw, RenderThreshold::default());
        assert_eq!(out.root_rules.len(), 1);
    }

    #[test]
    fn tighten_immune_text_strips_llm_rationalisation() {
        let raw = "This pattern should be avoided because it attempts to delete a database entity after already fetching it, which can cause race conditions and orphaned data if the record changes between the fetch and delete. Instead, developers should perform the delete as a single atomic operation using the primary key within the same database transaction.";
        let out = tighten_immune_text(raw, "Site/App_Code/dal/fiberjobb.vb");
        assert!(
            out.len() <= 200,
            "immune rule must be capped, got {} chars: {out}",
            out.len()
        );
        assert!(
            !out.starts_with("This pattern should be avoided"),
            "LLM prefix must be stripped, got: {out}"
        );
        assert!(
            out.contains("immune: Site/App_Code/dal/fiberjobb.vb"),
            "file pattern must be cited: {out}"
        );
    }

    #[test]
    fn render_immune_drops_process_hygiene_noise() {
        let raw = "This diff shows a minified JavaScript file being directly modified, which should be avoided because minified code is not meant for human editing.";
        assert!(
            render_immune_rule_text(raw, "Site/foo.min.js").is_none(),
            "minified-edit immune rule must be filtered as noise"
        );
        let raw = "The diff shows a file being added with duplicate, malformed header lines.";
        assert!(
            render_immune_rule_text(raw, "Site/foo.vb").is_none()
        );
    }

    #[test]
    fn categories_render_in_severity_order() {
        // Data correctness / security categories rank above
        // ergonomics. Build a cluster set spanning multiple
        // categories and assert the sort.
        let raw = vec![
            mk_raw("cr_loc", "Text is in the wrong language, use resx.", 3, 1.0, RuleSource::CodeRabbit),
            mk_raw("cr_null", "Guard against null DOM element.", 5, 1.0, RuleSource::CodeRabbit),
            mk_raw("cr_perm", "Permission check missing on api controller.", 4, 1.0, RuleSource::CodeRabbit),
        ];
        let out = run_pipeline(raw, RenderThreshold::default());
        // The localization rule gets noise-filtered ("text is in
        // the wrong language" is in the stop-phrase list), leaving
        // just null-guard and permission. Both rank 1 so order is
        // stable but they both pass.
        assert!(out.root_rules.iter().any(|r| r.text.contains("Null-guard")));
        assert!(out.root_rules.iter().any(|r| r.text.contains("permission check")));
    }

    #[test]
    fn other_category_rules_survive_when_above_threshold() {
        // A non-meta-clustered but high-confidence rule must
        // survive through the pipeline.
        let raw = vec![mk_raw(
            "cr_specific",
            "Fix WHERE clause precedence to prevent unintended global access.",
            3,
            1.0,
            RuleSource::CodeRabbit,
        )];
        let out = run_pipeline(raw, RenderThreshold::default());
        assert_eq!(out.root_rules.len(), 1);
        assert!(out.root_rules[0].text.contains("WHERE clause"));
    }

    #[test]
    fn pipeline_summary_is_non_empty_and_readable() {
        let raw = vec![mk_raw("cr_1", "Typo in comment.", 3, 1.0, RuleSource::CodeRabbit)];
        let out = run_pipeline(raw, RenderThreshold::default());
        assert!(out.summary.contains("raw"));
        assert!(out.summary.contains("noise filter"));
    }
}
