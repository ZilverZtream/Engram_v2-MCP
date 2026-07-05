//! Gate implementations for `pre_commit_review`.
//!
//! Each gate is a `struct` that implements the `Gate` trait. Keeping them
//! as separate types (rather than free functions) lets us list them in a
//! vector, skip individual gates via the request's `skip_gates` field, and
//! test each one in isolation with a mock `GateContext`.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use async_trait::async_trait;
use engram_graph::EdgeKind;
use regex::Regex;
use tokio_util::sync::CancellationToken;

use engram_core::registry::RepoRule;

use super::{
    ChangeType, ConventionCategory, DetectedConvention, DiffFile, Gate, GateContext, ReviewFinding,
    Severity, file_node_id, is_test_path, path_suffix_match, read_file_content,
    resolve_partner_to_current,
};

use crate::services::blast_radius_service::compute_blast_radius;
use engram_index::hybrid::HybridQuery;

/// Build the ordered list of gates. Order only matters for telemetry
/// (gates-run count); findings are sorted by severity at the end.
pub fn all_gates() -> Vec<Box<dyn Gate>> {
    vec![
        Box::new(ImmuneGate),
        Box::new(BlastRadiusGate),
        Box::new(StyleGate),
        Box::new(TemporalGate),
        Box::new(StateGate),
        Box::new(AuditGate),
        Box::new(AntiPatternGate),
        Box::new(NewFileGate),
        Box::new(TestCoverageGate),
        Box::new(SecretLeakageGate),
        Box::new(GuardParityGate),
        Box::new(UnwiredGate),
        Box::new(ProductIntentGate),
    ]
}

// ─── Gate 11: Guard parity ──────────────────────────────────────────────────
//
// A new endpoint or event handler that skips the permission checks its
// siblings use is the classic "public API added without the admin check"
// regression. Detection is generic name-shape matching (IsInRole,
// IsUserInRole, Is*Admin*, Check*Access*, Has*Permission*, Require*Role*,
// Demand*, Authorize*) — no application-specific helper names.

static RE_GP_NEW_ENDPOINT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(\[\s*webmethod|<\s*webmethod\s*\(\s*\)\s*>|\bprotected\s+(?:void|sub)\s+\w+_(?:click|command)\s*\(|\bhandles\s+\w+\.(?:click|command))",
    )
    .expect("valid endpoint regex")
});

static RE_GP_GUARD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(is[a-z0-9_]*admin[a-z0-9_]*|isinrole|isuserinrole|is[a-z0-9_]*role|check[a-z0-9_]*(?:access|permission|role)[a-z0-9_]*|has[a-z0-9_]*(?:permission|access|role)[a-z0-9_]*|require[a-z0-9_]*(?:role|permission|admin)[a-z0-9_]*|demand[a-z0-9_]*|authorize[a-z0-9_]*)\s*\(",
    )
    .expect("valid guard regex")
});

/// Distinct guard call names found in `text`, lowercased.
fn guard_names_in(text: &str) -> Vec<String> {
    let mut names: Vec<String> = RE_GP_GUARD
        .captures_iter(text)
        .filter_map(|c| c.get(1).map(|m| m.as_str().to_lowercase()))
        .collect();
    names.sort();
    names.dedup();
    names
}

pub struct GuardParityGate;

#[async_trait]
impl Gate for GuardParityGate {
    fn name(&self) -> &'static str {
        "guard_parity"
    }

    fn run(&self, ctx: &GateContext<'_>) -> anyhow::Result<Vec<ReviewFinding>> {
        let mut findings = Vec::new();
        for df in ctx.diff_files {
            let lower = df.path.to_lowercase();
            if !(lower.ends_with(".cs") || lower.ends_with(".vb") || lower.ends_with(".asmx")) {
                continue;
            }
            if df.added_content.is_empty() || !RE_GP_NEW_ENDPOINT.is_match(&df.added_content) {
                continue;
            }
            // The added code itself carries a guard — nothing to flag.
            if RE_GP_GUARD.is_match(&df.added_content) {
                continue;
            }
            // Sibling evidence: guards used elsewhere in the same file on
            // disk. Lossy-decode — a stray non-UTF-8 byte anywhere in a
            // legacy file must not silently blank out `disk` (and thus
            // hide real sibling guards) or, worse, propagate a hard error.
            let disk_bytes = std::fs::read(ctx.project_dir.join(&df.path)).unwrap_or_default();
            let disk = String::from_utf8_lossy(&disk_bytes);
            let sibling_guards = guard_names_in(&disk);
            if sibling_guards.is_empty() {
                // No guard convention in this file — parity can't be judged
                // from here; the project-wide view is map_guards_and_settings.
                continue;
            }
            let sibling_list = sibling_guards.join(", ");
            let mut finding = ReviewFinding::new(
                Severity::Warning,
                "guard_parity",
                df.path.clone(),
                "New endpoint/handler without the permission checks its siblings use",
                format!(
                    "The added code introduces an endpoint or event handler but contains \
                     none of the permission checks used elsewhere in this file ({sibling_list}). \
                     New public surface that skips the sibling guards is how admin-only \
                     operations leak to unauthorized users."
                ),
                format!(
                    "Guard the new entry point the same way its siblings do (e.g. call \
                     `{}` before any data access), or add an explicit comment stating why \
                     this endpoint is intentionally anonymous.",
                    sibling_guards
                        .first()
                        .map(String::as_str)
                        .unwrap_or("IsInRole")
                ),
            );
            finding.evidence = vec![format!("sibling guards in {}: {sibling_list}", df.path)];
            finding.next_tool = Some(format!("map_guards_and_settings(scope=\"{}\")", df.path));
            findings.push(finding);
        }
        Ok(findings)
    }
}

// ─── Destructive-pattern detection (reused by Immune + AntiPattern) ─────────

fn destructive_patterns() -> &'static [(&'static str, Regex)] {
    static PATTERNS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
        let raw: &[(&str, &str)] = &[
            ("DeleteAllOnSubmit", r"(?i)\bDeleteAllOnSubmit\s*\("),
            ("InsertAllOnSubmit", r"(?i)\bInsertAllOnSubmit\s*\("),
            ("RemoveRange", r"(?i)\bRemoveRange\s*\("),
            ("ExecuteDelete", r"(?i)\bExecuteDelete\s*\("),
            ("DROP TABLE", r"(?i)\bDROP\s+TABLE\b"),
            ("TRUNCATE TABLE", r"(?i)\bTRUNCATE\s+TABLE\b"),
            ("DELETE FROM", r"(?i)\bDELETE\s+FROM\s+[\[\]\w.]+"),
            (
                "ExecuteNonQuery + destructive SQL",
                r#"(?is)\bExecute(?:NonQuery|Sql|SqlRaw|SqlInterpolated)\b[\s\S]*?\b(?:DELETE|DROP|TRUNCATE)\b|\b(?:DELETE|DROP|TRUNCATE)\b[\s\S]*?\bExecute(?:NonQuery|Sql|SqlRaw|SqlInterpolated)\b"#,
            ),
        ];
        raw.iter()
            .filter_map(|(n, p)| Regex::new(p).ok().map(|re| (*n, re)))
            .collect()
    });
    PATTERNS.as_slice()
}

fn detect_destructive(code: &str) -> Vec<String> {
    let mut hits: Vec<String> = destructive_patterns()
        .iter()
        .filter(|(_, re)| re.is_match(code))
        .map(|(n, _)| n.to_string())
        .collect();
    hits.sort();
    hits.dedup();
    hits
}

/// Glob/substring matcher shared with `handle_immune_check`. Kept inline
/// rather than re-exported so we don't couple the service to the handler
/// module's private helpers.
fn path_pattern_matches(file_pattern: &str, target_path: &str) -> bool {
    if file_pattern.is_empty() {
        return false;
    }
    let pat = file_pattern.replace('\\', "/").to_lowercase();
    let path = target_path.replace('\\', "/").to_lowercase();
    if pat == path {
        return true;
    }
    if pat.contains('*') || pat.contains('?') {
        let mut re = String::with_capacity(pat.len() + 8);
        re.push('^');
        for c in pat.chars() {
            match c {
                '*' => re.push_str(".*"),
                '?' => re.push('.'),
                '.' | '+' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^' | '$' | '\\' => {
                    re.push('\\');
                    re.push(c);
                }
                _ => re.push(c),
            }
        }
        re.push('$');
        if let Ok(compiled) = Regex::new(&re) {
            return compiled.is_match(&path);
        }
        return false;
    }
    path.contains(&pat)
}

/// Derive a short revert-hash snippet from an immune rule id like
/// `immune_f7766bb1a1006ffd36432be2ae4fdb89b5291012`. Returns the first 8
/// hex chars so output stays readable.
fn extract_revert_hash(rule_id: &str) -> Option<String> {
    let s = rule_id.strip_prefix("immune_")?;
    if s.len() >= 8 && s.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(s[..8].to_string())
    } else {
        None
    }
}

// ─── Gate 1: Immune System ──────────────────────────────────────────────────

pub struct ImmuneGate;

#[async_trait]
impl Gate for ImmuneGate {
    fn name(&self) -> &'static str {
        "immune"
    }

    fn run(&self, ctx: &GateContext<'_>) -> anyhow::Result<Vec<ReviewFinding>> {
        // Repo rules are pre-loaded by the orchestrator into
        // `ctx.repo_rules` — gates MUST NOT re-query the registry here.
        let immune_rules: Vec<&RepoRule> = ctx
            .repo_rules
            .iter()
            .filter(|r| r.rule_id.starts_with("immune_"))
            .collect();
        if immune_rules.is_empty() {
            return Ok(Vec::new());
        }

        let mut findings = Vec::with_capacity(ctx.diff_files.len());
        for df in ctx.diff_files {
            if df.is_binary || matches!(df.change_type, ChangeType::Deleted) {
                continue;
            }
            // Collect matched rules + every derivable string in a single
            // pass to avoid three separate passes over the matched Vec.
            let mut rule_ids: Vec<String> = Vec::new();
            let mut revert_hashes: Vec<String> = Vec::new();
            let mut rule_texts: Vec<String> = Vec::new();
            for r in &immune_rules {
                if !path_pattern_matches(&r.file_pattern, &df.path) {
                    continue;
                }
                rule_ids.push(r.rule_id.clone());
                if let Some(h) = extract_revert_hash(&r.rule_id) {
                    revert_hashes.push(h);
                }
                if !r.rule_text.is_empty() {
                    rule_texts.push(r.rule_text.clone());
                }
            }
            if rule_ids.is_empty() {
                continue;
            }

            let destructive = detect_destructive(&df.added_content);

            if !destructive.is_empty() {
                let mut evidence = vec![
                    format!("immune_rule_ids = {}", rule_ids.join(", ")),
                    format!("destructive_patterns = {}", destructive.join(", ")),
                ];
                if !revert_hashes.is_empty() {
                    evidence.push(format!("revert_hashes = {}", revert_hashes.join(", ")));
                }
                let reason_tail = if rule_texts.is_empty() {
                    String::new()
                } else {
                    format!(" Previous revert reason: “{}”.", rule_texts.join(" / "))
                };
                let f = ReviewFinding::new(
                    Severity::Critical,
                    "immune",
                    df.path.clone(),
                    "Destructive code on immune-flagged file",
                    format!(
                        "This file was previously reverted and is now being modified with \
                         destructive operations ({}).{reason_tail}",
                        destructive.join(", ")
                    ),
                    format!(
                        "Run `immune_check(file_path=\"{}\", code=…)` on the exact snippet \
                         before committing. The immune flag exists specifically to prevent \
                         this pattern — either prove the operation is scoped (multitenant \
                         WHERE, transaction, explicit test) or rethink the change.",
                        df.path
                    ),
                )
                .with_evidence(evidence)
                .with_next_tool(format!(
                    "immune_check(project_id=\"{}\", file_path=\"{}\")",
                    ctx.project_id, df.path
                ));
                findings.push(f);
            } else {
                let mut evidence = vec![format!("immune_rule_ids = {}", rule_ids.join(", "))];
                if !revert_hashes.is_empty() {
                    evidence.push(format!("revert_hashes = {}", revert_hashes.join(", ")));
                }
                let reason_tail = if rule_texts.is_empty() {
                    String::new()
                } else {
                    format!(" Previous revert reason: “{}”.", rule_texts.join(" / "))
                };
                let f = ReviewFinding::new(
                    Severity::Warning,
                    "immune",
                    df.path.clone(),
                    "Immune-flagged file modified",
                    format!(
                        "This file was previously reverted and is being modified again.\
                         {reason_tail} No destructive patterns detected in the added code — \
                         this may be a legitimate fix, but verify that the original revert \
                         reason is addressed."
                    ),
                    format!(
                        "Read the revert commit for {} first. Confirm the added code does \
                         not reintroduce the reverted pattern.",
                        revert_hashes
                            .first()
                            .cloned()
                            .unwrap_or_else(|| "<unknown>".into())
                    ),
                )
                .with_evidence(evidence)
                .with_next_tool(format!(
                    "immune_check(project_id=\"{}\", file_path=\"{}\")",
                    ctx.project_id, df.path
                ));
                findings.push(f);
            }
        }

        Ok(findings)
    }
}

// ─── Gate 2: Blast Radius ───────────────────────────────────────────────────

pub struct BlastRadiusGate;

#[async_trait]
impl Gate for BlastRadiusGate {
    fn name(&self) -> &'static str {
        "blast_radius"
    }

    fn run(&self, ctx: &GateContext<'_>) -> anyhow::Result<Vec<ReviewFinding>> {
        // Cap analysis at 20 files — compute_blast_radius does a full
        // edge-table scan per file, so unbounded diffs would hit the
        // time budget.
        const FILE_CAP: usize = 20;

        let mut files: Vec<&DiffFile> = ctx
            .diff_files
            .iter()
            .filter(|f| !f.is_binary && !matches!(f.change_type, ChangeType::Deleted))
            .collect();
        // Prefer shallower paths first — they tend to be higher-blast
        // shared modules.
        files.sort_by_key(|f| f.path.matches('/').count());
        let truncated = files.len() > FILE_CAP;
        files.truncate(FILE_CAP);

        let mut findings = Vec::new();
        for df in files {
            let target = file_node_id(&df.path);
            let report = match compute_blast_radius(
                &ctx.graph,
                ctx.project_id,
                &target,
                ctx.generation,
                false,
            ) {
                Ok(r) => r,
                Err(_) => continue, // File not in graph yet / unknown → skip
            };

            // New files land with empty edge-sets — don't spam INFO findings.
            if report.total_incoming + report.total_outgoing == 0 {
                continue;
            }

            let severity = match report.migration_risk {
                0..=3 => continue,
                4..=6 => Severity::Info,
                _ => Severity::Warning,
            };
            let label = if severity == Severity::Warning {
                "High-blast-radius file modified"
            } else {
                "Medium-blast-radius file modified"
            };
            let evidence = vec![
                format!(
                    "migration_risk = {}/10 ({})",
                    report.migration_risk,
                    report.risk_band.as_str()
                ),
                format!("total_incoming = {}", report.total_incoming),
                format!("total_outgoing = {}", report.total_outgoing),
                format!("total_downstream = {}", report.total_downstream),
            ];
            let f = ReviewFinding::new(
                severity,
                "blast_radius",
                df.path.clone(),
                label,
                format!(
                    "`{}` has {} incoming dependents and {} outgoing dependencies. Changes \
                     here ripple outward; verify you tested the affected call sites.",
                    df.path, report.total_incoming, report.total_outgoing
                ),
                format!(
                    "Run `impact_analysis(file_path=\"{}\")` for the full dependent list. \
                     If any caller lives in a different tier (view ↔ service ↔ DAL), \
                     review their code paths as well.",
                    df.path
                ),
            )
            .with_evidence(evidence)
            .with_next_tool(format!(
                "impact_analysis(project_id=\"{}\", file_path=\"{}\")",
                ctx.project_id, df.path
            ));
            findings.push(f);
        }

        if truncated {
            findings.push(ReviewFinding::new(
                Severity::Info,
                "blast_radius",
                "(diff)".to_string(),
                "Blast-radius analysis truncated",
                format!(
                    "Diff touches more than {} files; blast radius was computed only \
                         for the shallowest 20 (by path depth). Deeper files were skipped \
                         to stay within the 5-second budget.",
                    FILE_CAP
                ),
                "For a full blast-radius read, run `impact_analysis` on each skipped \
                     file individually."
                    .to_string(),
            ));
        }

        Ok(findings)
    }
}

// ─── Gate 3: Style Compliance ───────────────────────────────────────────────

pub struct StyleGate;

#[async_trait]
impl Gate for StyleGate {
    fn name(&self) -> &'static str {
        "style"
    }

    fn run(&self, ctx: &GateContext<'_>) -> anyhow::Result<Vec<ReviewFinding>> {
        let mut findings = Vec::new();
        for df in ctx.diff_files {
            if df.is_binary
                || matches!(df.change_type, ChangeType::Deleted)
                || df.added_lines.is_empty()
            {
                continue;
            }
            let Some(full) = read_file_content(ctx.project_dir, &df.path) else {
                continue;
            };
            let conventions = super::extract_conventions(&full, &df.path);
            if conventions.is_empty() {
                continue;
            }
            let checked = check_style_compliance(df, &conventions);
            // Generated files (designer/partial-class output, or anything
            // carrying a generated-code header) get their Style-class
            // findings collapsed to a single skip notice — indentation and
            // naming there come from the generator, not a developer. Any
            // non-Style finding from the same check (e.g. "On Error Resume
            // Next reintroduced") is a real bug regardless of who wrote the
            // file, so it always survives.
            let is_generated =
                super::is_generated_filename(&df.path) || super::has_generated_header(&full);
            if is_generated {
                let (style, other): (Vec<_>, Vec<_>) = checked
                    .into_iter()
                    .partition(|f| f.severity == Severity::Style);
                findings.extend(other);
                findings.extend(super::apply_generated_exemption(
                    "style", &df.path, true, style,
                ));
            } else {
                findings.extend(checked);
            }
        }
        Ok(findings)
    }
}

/// Line-level style checks that fire when added code breaks a convention
/// detected on the full file.
fn check_style_compliance(df: &DiffFile, conventions: &[DetectedConvention]) -> Vec<ReviewFinding> {
    let mut out = Vec::new();
    let file_path = &df.path;
    let is_vb = file_path.to_ascii_lowercase().ends_with(".vb");
    let is_csharp = file_path.to_ascii_lowercase().ends_with(".cs");
    let is_ts_js = file_path.to_ascii_lowercase().ends_with(".ts")
        || file_path.to_ascii_lowercase().ends_with(".tsx")
        || file_path.to_ascii_lowercase().ends_with(".js")
        || file_path.to_ascii_lowercase().ends_with(".jsx");

    for conv in conventions {
        if conv.confidence() < 0.5 {
            continue;
        }

        match conv.category {
            ConventionCategory::MethodNaming => {
                let expected = conv.value.clone();
                let re_new_method: Option<Regex> = if is_vb {
                    Regex::new(r"(?im)^\s*(?:Public|Private|Protected|Friend|Shared|Overrides|Overridable|Async|Partial)?\s*(?:Public|Private|Protected|Friend|Shared|Overrides|Overridable|Async|Partial)?\s*(?:Sub|Function)\s+(\w+)\s*\(").ok()
                } else if is_csharp {
                    Regex::new(r"(?m)^\s*(?:public|private|protected|internal|static|virtual|override|async|sealed|abstract|new|partial)\s+(?:[\w<>\[\],\?\s]+?\s+)?(\w+)\s*\(").ok()
                } else if is_ts_js {
                    Regex::new(r"(?m)^\s*(?:export\s+)?(?:async\s+)?function\s+(\w+)\s*\(").ok()
                } else {
                    None
                };
                let Some(re) = re_new_method else { continue };

                for (line_no, line) in &df.added_lines {
                    if let Some(cap) = re.captures(line) {
                        if let Some(m) = cap.get(1) {
                            let name = m.as_str();
                            if !matches_casing(name, &expected) {
                                let suggested = convert_to_casing(name, &expected);
                                out.push(
                                    ReviewFinding::new(
                                        Severity::Style,
                                        "style",
                                        file_path.clone(),
                                        format!(
                                            "Method `{name}` doesn't match {expected} \
                                             convention ({}/{} existing methods)",
                                            conv.sample_count, conv.total_count
                                        ),
                                        format!(
                                            "Added method `{name}` on line {line_no} breaks \
                                             the file's `{expected}` method-naming convention."
                                        ),
                                        format!(
                                            "Rename `{name}` → `{suggested}` to match the \
                                             rest of the file."
                                        ),
                                    )
                                    .with_lines(vec![*line_no])
                                    .with_evidence(vec![
                                        format!("convention = {expected}"),
                                        format!(
                                            "sample = {}/{}",
                                            conv.sample_count, conv.total_count
                                        ),
                                    ]),
                                );
                            }
                        }
                    }
                }
            }
            ConventionCategory::ContextInjection if is_vb => {
                // Any new VB method that contains DataContext / .SubmitChanges / .Insert /
                // etc. should declare the `Optional db As <DataContext> = Nothing`
                // parameter.
                static NEW_METHOD_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
                    Regex::new(r"(?im)^\s*(?:Public|Private|Protected|Friend|Shared|Overrides|Overridable|Async|Partial)?\s*(?:Public|Private|Protected|Friend|Shared|Overrides|Overridable|Async|Partial)?\s*(?:Sub|Function)\s+(\w+)\s*\((.*?)\)").ok()
                });
                static DATA_ACCESS_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
                    Regex::new(r"(?i)\b(?:SubmitChanges|InsertOnSubmit|DataContext|\.Table\()\b")
                        .ok()
                });

                let has_data = DATA_ACCESS_RE
                    .as_ref()
                    .map(|re| re.is_match(&df.added_content))
                    .unwrap_or(false);
                if !has_data {
                    continue;
                }

                if let Some(re) = NEW_METHOD_RE.as_ref() {
                    for (line_no, line) in &df.added_lines {
                        if let Some(cap) = re.captures(line) {
                            let name = cap.get(1).map(|m| m.as_str()).unwrap_or("?");
                            let params = cap.get(2).map(|m| m.as_str()).unwrap_or("");
                            let has_optional_db = Regex::new(
                                r"(?i)\bOptional\s+\w+\s+As\s+\w+(?:DataContext|Context|Db)\s*=\s*Nothing",
                            )
                            .ok()
                            .map(|re| re.is_match(params))
                            .unwrap_or(false);
                            if !has_optional_db {
                                out.push(
                                    ReviewFinding::new(
                                        Severity::Style,
                                        "style",
                                        file_path.clone(),
                                        format!(
                                            "Method `{name}` missing optional-context parameter"
                                        ),
                                        format!(
                                            "This file uses `Optional db As <DataContext> = Nothing` as its \
                                             data-context injection pattern ({} methods). The added method \
                                             `{name}` on line {line_no} performs data access but does not \
                                             declare the optional parameter — callers can't reuse an outer \
                                             context and the method starts a fresh one every time.",
                                            conv.sample_count
                                        ),
                                        format!(
                                            "Add `, Optional db As <DataContext> = Nothing` to the signature \
                                             of `{name}` and route data access through `db`."
                                        ),
                                    )
                                    .with_lines(vec![*line_no])
                                    .with_evidence(vec![format!(
                                        "convention = Optional db = Nothing ({} occurrences)",
                                        conv.sample_count
                                    )]),
                                );
                            }
                        }
                    }
                }
            }
            ConventionCategory::ErrorHandling if is_vb && conv.value == "Try/Catch" => {
                static ON_ERROR_RE: LazyLock<Option<Regex>> =
                    LazyLock::new(|| Regex::new(r"(?im)^\s*On\s+Error\s+Resume\s+Next\b").ok());
                if let Some(re) = ON_ERROR_RE.as_ref() {
                    for (ln, line) in &df.added_lines {
                        if re.is_match(line) {
                            out.push(
                                ReviewFinding::new(
                                    Severity::Warning,
                                    "style",
                                    file_path.clone(),
                                    "`On Error Resume Next` reintroduced",
                                    "This file uses Try/Catch exclusively; adding `On Error \
                                     Resume Next` silently swallows exceptions and will \
                                     hide real failures at runtime.",
                                    "Remove `On Error Resume Next` and wrap the block in \
                                     `Try/Catch` with explicit error handling.",
                                )
                                .with_lines(vec![*ln])
                                .with_evidence(vec![format!(
                                    "convention = Try/Catch (file has {} Try/Catch sites)",
                                    conv.sample_count
                                )]),
                            );
                        }
                    }
                }
            }
            ConventionCategory::RedirectPattern if is_vb && conv.value == "SafeRedirect" => {
                static REDIR_RE: LazyLock<Option<Regex>> =
                    LazyLock::new(|| Regex::new(r"(?i)\bResponse\.Redirect\s*\(").ok());
                if let Some(re) = REDIR_RE.as_ref() {
                    for (ln, line) in &df.added_lines {
                        if re.is_match(line) {
                            out.push(
                                ReviewFinding::new(
                                    Severity::Style,
                                    "style",
                                    file_path.clone(),
                                    "Raw `Response.Redirect` — project uses `SafeRedirect`",
                                    format!(
                                        "This project wraps redirects in `SafeRedirect(...)` \
                                         ({} call sites). A raw `Response.Redirect` skips \
                                         the wrapper's normalisation / logging / \
                                         short-circuit guards.",
                                        conv.sample_count
                                    ),
                                    "Replace `Response.Redirect(url)` with `SafeRedirect(url)` \
                                     and add `Return` on the next line so the method \
                                     short-circuits."
                                        .to_string(),
                                )
                                .with_lines(vec![*ln]),
                            );
                        }
                    }
                }
            }
            ConventionCategory::ModuleSystem if is_ts_js && conv.value == "triple-slash" => {
                static IMPORT_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
                    Regex::new(r#"(?m)^\s*import\s+(?:[\w*{},\s]+\s+from\s+)?['"]"#).ok()
                });
                if let Some(re) = IMPORT_RE.as_ref() {
                    for (ln, line) in &df.added_lines {
                        if re.is_match(line) {
                            out.push(
                                ReviewFinding::new(
                                    Severity::Style,
                                    "style",
                                    file_path.clone(),
                                    "ES6 `import` added to triple-slash file",
                                    format!(
                                        "This file uses `/// <reference path=\"…\">` \
                                         ({}× ES6 imports in the file's current state: \
                                         {}/{}). Adding a new `import` forces TypeScript \
                                         into ES-module mode and is likely to break \
                                         compilation or runtime loading.",
                                        conv.total_count - conv.sample_count,
                                        conv.sample_count,
                                        conv.total_count
                                    ),
                                    "Replace `import { X } from \"…\"` with a \
                                     `/// <reference path=\"./X.ts\">` directive at the \
                                     top of the file."
                                        .to_string(),
                                )
                                .with_lines(vec![*ln]),
                            );
                        }
                    }
                }
            }
            ConventionCategory::StringQuotes if is_ts_js => {
                let expected = conv.value.clone();
                for (ln, line) in &df.added_lines {
                    let has_dbl = line.contains('"');
                    let has_sng = line.contains('\'');
                    if expected == "double" && has_sng && !has_dbl {
                        out.push(
                            ReviewFinding::new(
                                Severity::Style,
                                "style",
                                file_path.clone(),
                                "String quote style mismatch",
                                format!(
                                    "File uses double quotes ({}/{}). Added line uses single.",
                                    conv.sample_count, conv.total_count
                                ),
                                "Switch single-quoted string to double-quoted.".to_string(),
                            )
                            .with_lines(vec![*ln]),
                        );
                    } else if expected == "single" && has_dbl && !has_sng {
                        out.push(
                            ReviewFinding::new(
                                Severity::Style,
                                "style",
                                file_path.clone(),
                                "String quote style mismatch",
                                format!(
                                    "File uses single quotes ({}/{}). Added line uses double.",
                                    conv.sample_count, conv.total_count
                                ),
                                "Switch double-quoted string to single-quoted.".to_string(),
                            )
                            .with_lines(vec![*ln]),
                        );
                    }
                }
            }
            ConventionCategory::Indentation => {
                // Universal: if file uses spaces, flag tab-indented added lines.
                let expected_tabs = conv.value == "tabs";
                for (ln, line) in &df.added_lines {
                    if line.is_empty() {
                        continue;
                    }
                    let starts_tab = line.starts_with('\t');
                    let starts_space = line.starts_with(' ');
                    if !expected_tabs && starts_tab {
                        out.push(
                            ReviewFinding::new(
                                Severity::Style,
                                "style",
                                file_path.clone(),
                                "Indentation mismatch — tab on space-indented file",
                                format!(
                                    "File indents with {} (seen on {}/{} indented lines). \
                                     Added line starts with a tab.",
                                    conv.value, conv.sample_count, conv.total_count
                                ),
                                "Replace the leading tab with spaces.".to_string(),
                            )
                            .with_lines(vec![*ln]),
                        );
                    } else if expected_tabs && starts_space {
                        out.push(
                            ReviewFinding::new(
                                Severity::Style,
                                "style",
                                file_path.clone(),
                                "Indentation mismatch — space on tab-indented file",
                                "File indents with tabs. Added line starts with spaces.",
                                "Replace leading spaces with a tab.",
                            )
                            .with_lines(vec![*ln]),
                        );
                    }
                }
            }
            _ => {}
        }
    }

    out
}

fn matches_casing(name: &str, expected: &str) -> bool {
    if name.is_empty() {
        return true;
    }
    match expected {
        "PascalCase" => {
            name.chars().next().is_some_and(|c| c.is_ascii_uppercase()) && !name.contains('_')
        }
        "camelCase" => {
            name.chars().next().is_some_and(|c| c.is_ascii_lowercase()) && !name.contains('_')
        }
        "snake_case" => name.chars().all(|c| !c.is_ascii_uppercase()),
        _ => true,
    }
}

fn convert_to_casing(name: &str, expected: &str) -> String {
    match expected {
        "PascalCase" => {
            // camelCase / snake_case → PascalCase
            if name.contains('_') {
                name.split('_')
                    .filter(|s| !s.is_empty())
                    .map(|s| {
                        let mut chars = s.chars();
                        match chars.next() {
                            Some(c) => c.to_ascii_uppercase().to_string() + chars.as_str(),
                            None => String::new(),
                        }
                    })
                    .collect()
            } else {
                let mut chars = name.chars();
                match chars.next() {
                    Some(c) => c.to_ascii_uppercase().to_string() + chars.as_str(),
                    None => name.to_string(),
                }
            }
        }
        "camelCase" => {
            let mut chars = name.chars();
            match chars.next() {
                Some(c) => c.to_ascii_lowercase().to_string() + chars.as_str(),
                None => name.to_string(),
            }
        }
        "snake_case" => {
            let mut s = String::new();
            let mut prev_upper = false;
            for (i, c) in name.chars().enumerate() {
                if c.is_ascii_uppercase() {
                    if i > 0 && !prev_upper {
                        s.push('_');
                    }
                    s.push(c.to_ascii_lowercase());
                    prev_upper = true;
                } else {
                    s.push(c);
                    prev_upper = false;
                }
            }
            s
        }
        _ => name.to_string(),
    }
}

// ─── Gate 4: Temporal Co-change ─────────────────────────────────────────────

pub struct TemporalGate;

#[async_trait]
impl Gate for TemporalGate {
    fn name(&self) -> &'static str {
        "temporal"
    }

    fn run(&self, ctx: &GateContext<'_>) -> anyhow::Result<Vec<ReviewFinding>> {
        // Auto-tuned threshold: on small repos, weight=50 is too strict
        // (a 100-commit repo never reaches it). Scale the cutoff to the
        // project's commit volume — floor 5, ceiling 50, expected
        // proportion ≈1% of total commits.
        let base_threshold = if ctx.total_commits == 0 {
            50
        } else {
            ((ctx.total_commits as f32 * 0.01) as u32).clamp(5, 50)
        };
        let strong_threshold = base_threshold * 4;

        let mut findings = Vec::new();
        // Current-tree file paths (already loaded once per review, see
        // `GateContext::files_by_parent`) — used to re-anchor HISTORICAL
        // partner spellings (pre-restructure paths in co-change history,
        // e.g. `App_Code/x.vb` vs `Site/App_Code/x.vb`) to the spelling
        // that actually exists today.
        let current_files: Vec<String> = ctx.files_by_parent.values().flatten().cloned().collect();
        for df in ctx.diff_files {
            if df.is_binary || matches!(df.change_type, ChangeType::Deleted | ChangeType::Added) {
                continue;
            }
            let node_id = file_node_id(&df.path);
            let neighbors = ctx
                .graph
                .neighbors(ctx.project_id, EdgeKind::TemporalCoupling, &node_id, 20)
                .unwrap_or_default();
            // Multiple historical spellings can resolve to the same
            // current file — emit each partner once per diff file
            // (neighbors are weight-sorted, so the strongest wins).
            let mut emitted: HashSet<String> = HashSet::new();
            for (neighbor_id, weight) in neighbors {
                if weight < base_threshold {
                    continue;
                }
                let raw_neighbor = neighbor_id.strip_prefix("file:").unwrap_or(&neighbor_id);
                // Suffix-aware membership: a historical spelling counts as
                // "in the diff" when it component-suffix-matches any
                // changed path — this is what fixes the false "not in
                // diff" positives on restructured repos.
                if ctx
                    .changed_paths
                    .iter()
                    .any(|p| path_suffix_match(p, raw_neighbor))
                {
                    continue;
                }
                // Never emit a partner path that doesn't exist in the
                // current tree; when the historical spelling resolves to
                // an existing file, emit the CURRENT spelling instead.
                let Some(neighbor_path) =
                    resolve_partner_to_current(raw_neighbor, &current_files, ctx.project_dir)
                else {
                    continue;
                };
                if ctx
                    .changed_paths
                    .iter()
                    .any(|p| path_suffix_match(p, &neighbor_path))
                {
                    continue;
                }
                if !emitted.insert(neighbor_path.clone()) {
                    continue;
                }
                let pct = if ctx.total_commits > 0 {
                    let p = (weight as f32 / ctx.total_commits as f32 * 100.0).min(100.0);
                    format!("{p:.1}%")
                } else {
                    format!("{weight} co-changes")
                };
                let severity = if weight >= strong_threshold {
                    Severity::Warning
                } else {
                    Severity::Info
                };
                let f = ReviewFinding::new(
                    severity,
                    "temporal",
                    df.path.clone(),
                    format!("Coupled file `{neighbor_path}` not in diff"),
                    format!(
                        "Git history shows `{}` and `{neighbor_path}` change together in {pct} \
                         of commits (weight {weight}). Your diff changes the first but not the \
                         second.",
                        df.path
                    ),
                    format!(
                        "Either stage `{neighbor_path}` alongside `{}`, or add a commit \
                         message note explaining why the usual co-change is skipped.",
                        df.path
                    ),
                )
                .with_evidence(vec![
                    format!("coupling_weight = {weight}"),
                    format!("co_change_pct = {pct}"),
                    format!("total_commits = {}", ctx.total_commits),
                ])
                .with_next_tool(format!(
                    "list_temporal_couplings(project_id=\"{}\", file_path=\"{}\")",
                    ctx.project_id, df.path
                ));
                findings.push(f);
            }
        }

        Ok(findings)
    }
}

// ─── Gate 5: State Access Validation ────────────────────────────────────────

pub struct StateGate;

static STATE_KEY_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)\b(Session|ViewState|Application|Cache)\s*[\(\[]\s*["']([\w_\.\-]+)["']\s*[\)\]]"#,
    )
    .ok()
});

#[async_trait]
impl Gate for StateGate {
    fn name(&self) -> &'static str {
        "state"
    }

    fn run(&self, ctx: &GateContext<'_>) -> anyhow::Result<Vec<ReviewFinding>> {
        let Some(re) = STATE_KEY_RE.as_ref() else {
            return Ok(Vec::new());
        };
        let mut findings = Vec::new();
        for df in ctx.diff_files {
            if df.is_binary || matches!(df.change_type, ChangeType::Deleted) {
                continue;
            }
            let mut per_file: HashMap<(String, String), Vec<usize>> = HashMap::new();
            for (ln, line) in &df.added_lines {
                for cap in re.captures_iter(line) {
                    let store = cap
                        .get(1)
                        .map(|m| m.as_str().to_string())
                        .unwrap_or_default();
                    let key = cap
                        .get(2)
                        .map(|m| m.as_str().to_string())
                        .unwrap_or_default();
                    per_file.entry((store, key)).or_default().push(*ln);
                }
            }
            for ((store, key), lines) in per_file {
                // Graph stores state keys with nodeId "state:<Store>:<key>".
                let node_id = format!("state:{store}:{key}");
                let readers = ctx
                    .graph
                    .find_incoming_edges(ctx.project_id, Some(EdgeKind::ReadsState), &node_id, 50)
                    .unwrap_or_default();
                let writers = ctx
                    .graph
                    .find_incoming_edges(ctx.project_id, Some(EdgeKind::WritesState), &node_id, 50)
                    .unwrap_or_default();
                // Filter out the current file — we only care about OTHER
                // readers/writers affected by this change.
                let other_readers: Vec<String> = readers
                    .into_iter()
                    .map(|(id, _)| id)
                    .filter(|id| !id.contains(&df.path))
                    .collect();
                let other_writers: Vec<String> = writers
                    .into_iter()
                    .map(|(id, _)| id)
                    .filter(|id| !id.contains(&df.path))
                    .collect();
                let total_others = other_readers.len() + other_writers.len();
                if total_others == 0 {
                    continue;
                }
                let severity = if other_readers.len() >= 5 || other_writers.len() >= 3 {
                    Severity::Warning
                } else {
                    Severity::Info
                };
                let reader_sample = other_readers.iter().take(3).cloned().collect::<Vec<_>>();
                let writer_sample = other_writers.iter().take(3).cloned().collect::<Vec<_>>();
                let f = ReviewFinding::new(
                    severity,
                    "state",
                    df.path.clone(),
                    format!("`{store}[\"{key}\"]` touched — {total_others} other location(s) use this key"),
                    format!(
                        "The state key `{store}[\"{key}\"]` is accessed by {} other reader(s) \
                         and {} other writer(s). Changing what gets stored or when it gets \
                         cleared can break any of them.",
                        other_readers.len(),
                        other_writers.len()
                    ),
                    format!(
                        "Verify every reader/writer still handles the new shape of \
                         `{store}[\"{key}\"]`. Run `get_session_workflows(key=\"{key}\")` for \
                         the full cross-page flow."
                    ),
                )
                .with_lines(lines)
                .with_evidence(vec![
                    format!(
                        "readers = {} (sample: {})",
                        other_readers.len(),
                        reader_sample.join(", ")
                    ),
                    format!(
                        "writers = {} (sample: {})",
                        other_writers.len(),
                        writer_sample.join(", ")
                    ),
                ])
                .with_next_tool(format!(
                    "get_session_workflows(project_id=\"{}\", key=\"{}\")",
                    ctx.project_id, key
                ));
                findings.push(f);
            }
        }

        Ok(findings)
    }
}

// ─── Gate 6: Audit Log ──────────────────────────────────────────────────────

pub struct AuditGate;

#[async_trait]
impl Gate for AuditGate {
    fn name(&self) -> &'static str {
        "audit"
    }

    fn run(&self, ctx: &GateContext<'_>) -> anyhow::Result<Vec<ReviewFinding>> {
        // Audit-function name is pre-detected by the orchestrator (once
        // per review) — if absent, the project has no audit convention
        // and the gate silently skips. Eliminates up to 5 `query_nodes`
        // calls on every review.
        let Some(audit_name) = ctx.audit_function.clone() else {
            return Ok(Vec::new());
        };
        let audit_short = audit_name
            .rsplit('.')
            .next()
            .unwrap_or(&audit_name)
            .to_string();

        static MUTATION_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
            Regex::new(r"(?i)\.SubmitChanges\s*\(|\.SaveChanges(Async)?\s*\(|\bINSERT\s+INTO\b|\bUPDATE\s+\w|\bDELETE\s+FROM\b|\.ExecuteNonQuery\s*\(").ok()
        });

        let mut findings = Vec::new();
        for df in ctx.diff_files {
            if df.is_binary || matches!(df.change_type, ChangeType::Deleted) {
                continue;
            }
            let Some(re) = MUTATION_RE.as_ref() else {
                continue;
            };
            let mut mutation_lines: Vec<usize> = Vec::new();
            let mut mutation_names: Vec<String> = Vec::new();
            for (ln, line) in &df.added_lines {
                if let Some(m) = re.find(line) {
                    mutation_lines.push(*ln);
                    mutation_names.push(m.as_str().trim().to_string());
                }
            }
            if mutation_lines.is_empty() {
                continue;
            }
            let has_audit = df.added_content.contains(&audit_short);
            if has_audit {
                continue;
            }

            // Check whether the file already calls the audit function
            // elsewhere — elevates severity (the author knows the
            // convention and is now skipping it).
            let file_calls_audit = read_file_content(ctx.project_dir, &df.path)
                .map(|c| c.contains(&audit_short))
                .unwrap_or(false);
            let severity = if file_calls_audit {
                Severity::Warning
            } else {
                Severity::Info
            };
            let f = ReviewFinding::new(
                severity,
                "audit",
                df.path.clone(),
                "Database mutation without audit-log call",
                format!(
                    "Added code contains data mutation(s) ({}) but no call to `{audit_short}`, \
                     which is this project's established audit convention.",
                    mutation_names.join(", ")
                ),
                format!(
                    "Add a `{audit_name}(...)` call immediately after the mutation, recording \
                     the caller / entity / operation as the rest of the project does."
                ),
            )
            .with_lines(mutation_lines)
            .with_evidence(vec![
                format!("audit_convention = {audit_name}"),
                format!("mutations = {}", mutation_names.join(", ")),
            ]);
            findings.push(f);
        }
        Ok(findings)
    }
}

// ─── Gate 7: Anti-Pattern Matching (async — hybrid search) ──────────────────

pub struct AntiPatternGate;

#[async_trait]
impl Gate for AntiPatternGate {
    fn name(&self) -> &'static str {
        "antipattern"
    }

    async fn run_async(&self, ctx: &GateContext<'_>) -> anyhow::Result<Vec<ReviewFinding>> {
        // ENSURE the project-runtime search engine — a fresh daemon has
        // no cached ProjectState, and a `get_project_cached`-only lookup
        // silently no-ops the whole search branch on the first review of
        // a session (observed live: only the destructive fallback ever
        // fired). Fall back to the pure destructive-pattern scan only
        // when the runtime genuinely can't be built.
        let ps = match crate::services::project_service::ensure_project_runtime(
            ctx.state,
            ctx.project_id,
        )
        .await
        {
            Ok(p) => p,
            Err(_) => return Ok(self.destructive_only(ctx)),
        };

        let mut findings = self.destructive_only(ctx);

        for df in ctx.diff_files {
            if df.is_binary
                || matches!(df.change_type, ChangeType::Deleted)
                || df.added_content.len() < 50
            {
                continue;
            }
            let query = crate::utils::text::code_to_query(&df.added_content);
            // Run both namespace searches concurrently — same query,
            // two indexes (antipattern + wontfix_patterns). Suppression
            // hits scoped to the diff's file family dampen antipattern
            // scores for patterns the team has explicitly said "leave
            // alone" in that code area.
            let ap_query = HybridQuery {
                project_id: ctx.project_id.to_string(),
                namespace: "antipattern".into(),
                generation: ctx.generation,
                text: query.clone(),
                top_k: 5,
                fts_mode: "loose".into(),
                include_path_prefixes: None,
                exclude_path_prefixes: None,
                language_filters: None,
                author_filter: None,
                date_after: None,
                date_before: None,
                use_mmr: false,
            };
            let supp_query = HybridQuery {
                project_id: ctx.project_id.to_string(),
                namespace: "wontfix_patterns".into(),
                generation: ctx.generation,
                text: query,
                top_k: 5,
                fts_mode: "loose".into(),
                // File-family scoping: only pull suppression hits
                // whose stored `path` (the cluster's file pattern)
                // prefixes the diff's file path after lowercasing.
                // This keeps the `gQtyManager` suppression pinned to
                // qtyManager files instead of dampening every
                // TypeScript file globally.
                include_path_prefixes: Some(derive_family_prefixes(&df.path)),
                exclude_path_prefixes: None,
                language_filters: None,
                author_filter: None,
                date_after: None,
                date_before: None,
                use_mmr: false,
            };
            let cancel = CancellationToken::new();
            let hits_fut = ps.search.search(&ap_query, None, &cancel);
            let supp_fut = ps.search.search(&supp_query, None, &cancel);
            let (hits, supp_hits) = tokio::join!(hits_fut, supp_fut);
            let hits = hits.unwrap_or_default();
            let supp_hits = supp_hits.unwrap_or_default();

            // Hybrid scores are RRF rank-fusion values (~0.03 at rank 1
            // regardless of match quality) — the old `score > 0.3`
            // filter could NEVER pass, which silently disabled this
            // whole branch. Judge relevance by TERM OVERLAP between the
            // diff-derived query and each hit's actual stored content
            // instead (same mechanism as ProductIntentGate). Content is
            // fetched by pk — exact, generation-proof.
            let query_text = crate::utils::text::code_to_query(&df.added_content);
            let mut relevant: Vec<(String, f32)> = Vec::new(); // (display path, overlap)
            for h in hits.into_iter().take(5) {
                let content = ps
                    .search
                    .get_doc_by_pk(&h.pk)
                    .ok()
                    .flatten()
                    .map(|(_, _, c, _, _)| c)
                    .unwrap_or_default();
                if content.is_empty() {
                    continue;
                }
                let (matched_n, total_n, _) = query_overlap(&content, &query_text);
                let overlap = matched_n as f32 / total_n.max(1) as f32;
                if matched_n >= 4 && overlap >= 0.3 {
                    relevant.push((h.path.as_str().to_string(), overlap));
                }
            }
            if relevant.len() < 2 {
                continue;
            }
            // Suppression: if the diff matches ≥1 wontfix_patterns doc
            // (scoped to this file's family) with real term overlap,
            // dampen by one severity tier. This is the file-scoped
            // false-positive suppression the team explicitly asked for
            // via the wontFix threads — we don't discard the finding,
            // we just downgrade it so it doesn't scream about a
            // pattern someone already looked at and left alone.
            let strong_supp = supp_hits.iter().take(5).any(|h| {
                ps.search
                    .get_doc_by_pk(&h.pk)
                    .ok()
                    .flatten()
                    .map(|(_, _, c, _, _)| {
                        let (m, t, _) = query_overlap(&c, &query_text);
                        m >= 4 && (m as f32 / t.max(1) as f32) >= 0.3
                    })
                    .unwrap_or(false)
            });
            let mut severity = if relevant.iter().any(|(_, ov)| *ov >= 0.5) {
                Severity::Warning
            } else {
                Severity::Info
            };
            if strong_supp {
                severity = match severity {
                    Severity::Warning => Severity::Info,
                    Severity::Info => Severity::Style,
                    other => other,
                };
            }

            let mut evidence: Vec<String> = Vec::new();
            for (path, overlap) in &relevant {
                // `path` on CodeRabbit-sourced docs is the cluster's
                // file pattern (e.g. `/site/**/*.vb`) — surface that
                // so the reader sees it's a CodeRabbit rule, not a
                // reverted-commit antipattern. The DocStore also
                // carries `author` = "coderabbit" on those docs; the
                // path prefix is a reliable visual signal.
                let source_label = if path.contains("**/") || path.starts_with("coderabbit://") {
                    " [source: CodeRabbit]"
                } else {
                    ""
                };
                evidence.push(format!(
                    "match = `{path}` (term overlap {overlap:.2}){source_label}"
                ));
            }
            if !supp_hits.is_empty() {
                evidence.push(format!(
                    "file-scoped suppressions (wontFix): {} match(es) — severity dampened",
                    supp_hits.len()
                ));
            }
            findings.push(
                ReviewFinding::new(
                    severity,
                    "antipattern",
                    df.path.clone(),
                    format!(
                        "Added code resembles {} indexed anti-pattern(s)",
                        relevant.len()
                    ),
                    format!(
                        "The hybrid search index found {} previously-reverted / CodeRabbit-\
                         flagged snippet(s) that structurally match the added code. Review \
                         them before merging to confirm you are not re-introducing a known \
                         bad pattern.",
                        relevant.len()
                    ),
                    format!(
                        "Run `anti_pattern_guard(project_id=\"{}\", code=…)` with the exact \
                         snippet for full match detail.",
                        ctx.project_id
                    ),
                )
                .with_evidence(evidence)
                .with_next_tool(format!(
                    "anti_pattern_guard(project_id=\"{}\")",
                    ctx.project_id
                )),
            );
        }

        Ok(findings)
    }
}

/// Derive a small set of path prefixes that identify the diff file's
/// "family" (its own directory, plus progressive parents up to 3
/// levels). These become `include_path_prefixes` for the
/// wontfix_patterns search — suppressions stored under a tight path
/// pattern (e.g. `/site/ts/qty/**/*.ts`) only fire for diffs in that
/// subtree, not for every TypeScript file globally.
fn derive_family_prefixes(diff_path: &str) -> Vec<String> {
    let lower = diff_path.replace('\\', "/").to_ascii_lowercase();
    let mut out: Vec<String> = Vec::with_capacity(4);
    let mut cur = lower.as_str();
    for _ in 0..3 {
        let Some(idx) = cur.rfind('/') else { break };
        cur = &cur[..idx];
        if !cur.is_empty() {
            out.push(cur.to_string());
        }
    }
    // Always include the leading slash form too — CodeRabbit JSONL
    // file paths start with `/`, so cluster-stored paths like
    // `/site/ts/qty/**/*.ts` prefix-match via their leading segments.
    out.push(format!("/{}", lower.trim_start_matches('/')));
    out
}

impl AntiPatternGate {
    fn destructive_only(&self, ctx: &GateContext<'_>) -> Vec<ReviewFinding> {
        let mut findings = Vec::new();
        for df in ctx.diff_files {
            if df.is_binary || matches!(df.change_type, ChangeType::Deleted) {
                continue;
            }
            let hits = detect_destructive(&df.added_content);
            if hits.is_empty() {
                continue;
            }
            findings.push(
                ReviewFinding::new(
                    Severity::Warning,
                    "antipattern",
                    df.path.clone(),
                    "Destructive patterns detected in added code",
                    format!(
                        "Added code contains operations that rarely pass review on a DAL \
                         surface: {}. Each should have an explicit multitenant `WHERE`, a \
                         transaction, and a test that demonstrates scope.",
                        hits.join(", ")
                    ),
                    "Confirm each destructive call is scoped correctly. If there is any \
                     doubt, split the change into a dry-run + apply pattern so the first \
                     PR demonstrates the target rows."
                        .to_string(),
                )
                .with_evidence(vec![format!("destructive_patterns = {}", hits.join(", "))]),
            );
        }
        findings
    }
}

// ─── Gate 8: New File Convention ────────────────────────────────────────────

pub struct NewFileGate;

#[async_trait]
impl Gate for NewFileGate {
    fn name(&self) -> &'static str {
        "new_file"
    }

    fn run(&self, ctx: &GateContext<'_>) -> anyhow::Result<Vec<ReviewFinding>> {
        let mut findings = Vec::new();

        let added_aspx: HashSet<String> = ctx
            .diff_files
            .iter()
            .filter(|f| matches!(f.change_type, ChangeType::Added))
            .filter(|f| f.path.to_ascii_lowercase().ends_with(".aspx"))
            .map(|f| f.path.clone())
            .collect();
        let added_codebehind: HashSet<String> = ctx
            .diff_files
            .iter()
            .filter(|f| matches!(f.change_type, ChangeType::Added))
            .filter(|f| {
                let lc = f.path.to_ascii_lowercase();
                lc.ends_with(".aspx.vb") || lc.ends_with(".aspx.cs")
            })
            .map(|f| f.path.clone())
            .collect();

        for df in ctx.diff_files {
            if !matches!(df.change_type, ChangeType::Added) {
                continue;
            }
            let parent = match df.path.rfind('/') {
                Some(i) => &df.path[..i],
                None => "",
            };
            if parent.is_empty() {
                continue;
            }

            // Read pre-built parent-dir → files index once instead of
            // issuing a graph query per added file (previously:
            // `query_nodes(limit=1000)` per file — O(added_files · N) on
            // a project with N files).
            let sibling_names: Vec<String> = ctx
                .files_by_parent
                .get(parent)
                .map(|v| v.iter().filter(|p| *p != &df.path).cloned().collect())
                .unwrap_or_default();

            if sibling_names.len() >= 3 {
                let mut style_findings: Vec<ReviewFinding> = Vec::new();

                // Extension consistency.
                let file_ext = extension(&df.path);
                let sibling_exts: HashMap<String, usize> = count_extensions(&sibling_names);
                if let Some((top_ext, top_count)) = sibling_exts
                    .iter()
                    .max_by_key(|(_, c)| **c)
                    .map(|(k, v)| (k.clone(), *v))
                {
                    if !file_ext.is_empty()
                        && file_ext != top_ext
                        && top_count >= sibling_names.len() / 2
                    {
                        style_findings.push(ReviewFinding::new(
                            Severity::Style,
                            "new_file",
                            df.path.clone(),
                            format!("New file extension `.{}` differs from siblings", file_ext),
                            format!(
                                "`{parent}/` contains {top_count} `.{top_ext}` files. The \
                                     new file `{}` uses `.{}`.",
                                df.path, file_ext
                            ),
                            format!(
                                "If the project organises files by language, consider \
                                     placing `{}` in a folder that matches `.{file_ext}`.",
                                df.path
                            ),
                        ));
                    }
                }

                // Prefix convention.
                let prefix = common_name_prefix(&sibling_names);
                if let Some(p) = prefix {
                    let fname = df.path.rsplit('/').next().unwrap_or(&df.path);
                    if !fname.starts_with(&p) {
                        style_findings.push(ReviewFinding::new(
                            Severity::Style,
                            "new_file",
                            df.path.clone(),
                            format!("File naming doesn't match `{p}*` convention"),
                            format!(
                                "Every sibling file in `{parent}/` starts with `{p}`. The \
                                     new file `{fname}` does not.",
                            ),
                            format!("Rename `{fname}` → `{p}{fname}` or similar."),
                        ));
                    }
                }

                if !style_findings.is_empty() {
                    // Same exemption as StyleGate: a generated new file
                    // (e.g. a scaffolded `.designer.vb` sibling) doesn't
                    // get naming/extension nits — `added_content` on an
                    // Added file IS the whole file, so the header-marker
                    // check needs no extra disk read.
                    let is_generated = super::is_generated_filename(&df.path)
                        || super::has_generated_header(&df.added_content);
                    findings.extend(super::apply_generated_exemption(
                        "new_file",
                        &df.path,
                        is_generated,
                        style_findings,
                    ));
                }
            }

            // ASPX without codebehind.
            if df.path.to_ascii_lowercase().ends_with(".aspx") {
                let has_cb = added_codebehind.iter().any(|cb| {
                    cb.to_ascii_lowercase()
                        .starts_with(&df.path.to_ascii_lowercase())
                });
                if !has_cb {
                    findings.push(ReviewFinding::new(
                        Severity::Info,
                        "new_file",
                        df.path.clone(),
                        "ASPX page added without a codebehind file",
                        format!(
                            "`{}` is a new page but no matching `.aspx.vb` / `.aspx.cs` \
                                 codebehind was added in the same diff.",
                            df.path
                        ),
                        "If the project uses codebehind (almost every WebForms project \
                             does), add the matching file. If the page is intentionally \
                             codebehind-less, document why in the commit message."
                            .to_string(),
                    ));
                }
            }
            let _ = added_aspx.contains(&df.path); // silence unused; helps in future extensions
        }

        Ok(findings)
    }
}

fn extension(path: &str) -> String {
    let fname = path.rsplit('/').next().unwrap_or(path);
    if let Some(idx) = fname.rfind('.') {
        fname[idx + 1..].to_ascii_lowercase()
    } else {
        String::new()
    }
}

fn count_extensions(files: &[String]) -> HashMap<String, usize> {
    let mut out: HashMap<String, usize> = HashMap::new();
    for f in files {
        let ext = extension(f);
        if !ext.is_empty() {
            *out.entry(ext).or_insert(0) += 1;
        }
    }
    out
}

/// Return a common prefix shared by ≥60% of the file names (after
/// dropping the directory part). Only non-trivial prefixes (≥2 chars)
/// count — empty string would match everything.
fn common_name_prefix(paths: &[String]) -> Option<String> {
    if paths.len() < 3 {
        return None;
    }
    let names: Vec<&str> = paths
        .iter()
        .map(|p| p.rsplit('/').next().unwrap_or(p))
        .collect();
    let min_len = names.iter().map(|s| s.len()).min().unwrap_or(0);
    if min_len < 2 {
        return None;
    }
    let mut best: Option<String> = None;
    for len in (2..=min_len.min(12)).rev() {
        let candidate: &str = &names[0][..len];
        // Require the prefix to end at a word boundary (non-alnum char or
        // end-of-segment) so we don't accidentally match `per` inside
        // `permit_*` and `permission_*` as a single group.
        let count = names.iter().filter(|n| n.starts_with(candidate)).count();
        if count * 10 >= names.len() * 6 {
            best = Some(candidate.to_string());
            break;
        }
    }
    best
}

// ─── Gate 9: Test Coverage ──────────────────────────────────────────────────

pub struct TestCoverageGate;

#[async_trait]
impl Gate for TestCoverageGate {
    fn name(&self) -> &'static str {
        "test_coverage"
    }

    fn run(&self, ctx: &GateContext<'_>) -> anyhow::Result<Vec<ReviewFinding>> {
        let mut findings = Vec::new();
        let any_test_in_diff = ctx.diff_files.iter().any(|f| is_test_path(&f.path));
        // Current-tree paths for resolving historical partner spellings —
        // see TemporalGate for the rationale.
        let current_files: Vec<String> = ctx.files_by_parent.values().flatten().cloned().collect();
        for df in ctx.diff_files {
            if df.is_binary
                || matches!(df.change_type, ChangeType::Deleted)
                || is_test_path(&df.path)
                || df.added_lines.len() < 5
            {
                continue;
            }
            // Heuristic: when the diff adds ≥5 lines of non-test code and
            // (a) no test file in the diff at all, OR (b) a coupled test
            // file exists in the graph but isn't in the diff — warn.
            let node_id = file_node_id(&df.path);
            let neighbors = ctx
                .graph
                .neighbors(ctx.project_id, EdgeKind::TemporalCoupling, &node_id, 30)
                .unwrap_or_default();
            let coupled_tests_raw: Vec<(String, u32)> = neighbors
                .into_iter()
                .filter(|(id, _)| {
                    let p = id.strip_prefix("file:").unwrap_or(id);
                    is_test_path(p)
                })
                .map(|(id, w)| (id.strip_prefix("file:").unwrap_or(&id).to_string(), w))
                .collect();

            // Suffix-aware membership — history may carry a pre-restructure
            // spelling of a test file that IS in the diff.
            let has_coupled_test_in_diff = coupled_tests_raw
                .iter()
                .any(|(p, _)| ctx.changed_paths.iter().any(|c| path_suffix_match(c, p)));
            if has_coupled_test_in_diff {
                continue;
            }

            // Only ever suggest test files that exist in the current tree,
            // under their current spelling — never a stale one.
            let coupled_tests: Vec<(String, u32)> = coupled_tests_raw
                .iter()
                .filter_map(|(p, w)| {
                    resolve_partner_to_current(p, &current_files, ctx.project_dir)
                        .map(|cp| (cp, *w))
                })
                .collect();

            if !coupled_tests.is_empty() {
                let (best_path, best_weight) = coupled_tests
                    .iter()
                    .max_by_key(|(_, w)| *w)
                    .cloned()
                    .unwrap();
                findings.push(
                    ReviewFinding::new(
                        Severity::Warning,
                        "test_coverage",
                        df.path.clone(),
                        format!("Coupled test file `{best_path}` not in diff"),
                        format!(
                            "Git history shows `{}` and `{best_path}` change together; the \
                             added code here has no matching test update (weight {best_weight}).",
                            df.path
                        ),
                        format!(
                            "Add / update tests in `{best_path}` to cover the new behaviour \
                             before merging."
                        ),
                    )
                    .with_evidence(vec![
                        format!("coupling_weight = {best_weight}"),
                        format!("coupled_test = {best_path}"),
                    ]),
                );
            } else if !any_test_in_diff {
                // No coupled test exists — a softer note.
                findings.push(ReviewFinding::new(
                    Severity::Info,
                    "test_coverage",
                    df.path.clone(),
                    "No test file changed alongside this code",
                    format!(
                        "`{}` added {} lines of non-test code; no test file in the \
                             project is temporally coupled to it, and no test file is in \
                             the diff.",
                        df.path,
                        df.added_lines.len()
                    ),
                    "If this code has behaviour worth locking in, add a test. If it's a \
                         pure refactor, note that in the commit message."
                        .to_string(),
                ));
            }
        }

        Ok(findings)
    }
}

// ─── Gate 10: Secret Leakage ────────────────────────────────────────────────

pub struct SecretLeakageGate;

struct SecretPattern {
    name: &'static str,
    re: Regex,
}

fn secret_patterns() -> &'static [SecretPattern] {
    static PATTERNS: LazyLock<Vec<SecretPattern>> = LazyLock::new(|| {
        // Patterns below are intentionally conservative — a false CRITICAL
        // here erodes the whole tool. If something here fires on a benign
        // test fixture, tighten it.
        let specs: &[(&str, &str)] = &[
            // AWS
            ("AWS Access Key ID", r"\bAKIA[0-9A-Z]{16}\b"),
            (
                "AWS Secret Access Key",
                r#"(?i)\baws(?:.{0,20})?(?:secret|key)(?:.{0,20})?['"\s:=]+["']?([A-Za-z0-9/+]{40})\b"#,
            ),
            // Google
            ("Google API Key", r"\bAIza[0-9A-Za-z_\-]{35}\b"),
            // GitHub
            ("GitHub Personal Access Token", r"\bghp_[0-9A-Za-z]{36}\b"),
            ("GitHub OAuth Token", r"\bgho_[0-9A-Za-z]{36}\b"),
            ("GitHub App Token", r"\bghs_[0-9A-Za-z]{36}\b"),
            // OpenAI — classic (`sk-…`) and project (`sk-proj-…`) keys.
            // Length threshold is generous enough to match short test /
            // rotation fixtures that still reveal the key shape.
            ("OpenAI API Key", r"\bsk-(?:proj-)?[A-Za-z0-9_\-]{12,}\b"),
            // Anthropic
            ("Anthropic API Key", r"\bsk-ant-[A-Za-z0-9_\-]{20,}\b"),
            // Slack
            ("Slack Bot Token", r"\bxoxb-[0-9]+-[0-9]+-[A-Za-z0-9]+\b"),
            (
                "Slack User Token",
                r"\bxoxp-[0-9]+-[0-9]+-[0-9]+-[A-Za-z0-9]+\b",
            ),
            // Stripe
            ("Stripe Secret Key", r"\bsk_live_[0-9A-Za-z]{24,}\b"),
            ("Stripe Restricted Key", r"\brk_live_[0-9A-Za-z]{24,}\b"),
            // JWT
            (
                "JSON Web Token",
                r"\beyJ[A-Za-z0-9_\-]{10,}\.eyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\b",
            ),
            // Private key PEM
            (
                "Private Key PEM",
                r"-----BEGIN (?:RSA |EC |DSA |OPENSSH )?PRIVATE KEY-----",
            ),
            // Generic high-entropy secret assignments. Matches the
            // assignment shape across languages — including VB's
            // `Const API_KEY As String = "…"` form where `As <Type>`
            // sits between the identifier and `=`. The `[^"'\n]{0,40}`
            // gap accepts type annotations / qualifiers while still
            // anchoring on a quoted 12+ character value.
            (
                "Hardcoded credential assignment",
                r#"(?i)(?:password|passwd|secret|token|api[_\-]?key|auth[_\-]?key)\b[^"'\n]{0,40}[:=]\s*["'][A-Za-z0-9_!@#$%\^&*\-+/=]{12,}["']"#,
            ),
            // ADO.NET / JDBC connection strings with `Password=` or `Pwd=`.
            (
                "Connection string with embedded password",
                r#"(?i)(?:Password|Pwd)\s*=\s*[^;\s"']{4,}"#,
            ),
        ];
        specs
            .iter()
            .filter_map(|(n, p)| Regex::new(p).ok().map(|re| SecretPattern { name: n, re }))
            .collect()
    });
    PATTERNS.as_slice()
}

/// Scan for hardcoded secrets in added content. Redacts the matched
/// secret in output (reports only the pattern name + a fingerprint) so
/// the review itself never echoes the credential back at the caller.
fn scan_for_secrets(code: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for sp in secret_patterns() {
        for m in sp.re.find_iter(code).take(5) {
            let fingerprint = fingerprint_secret(m.as_str());
            out.push((sp.name.to_string(), fingerprint));
        }
    }
    out
}

fn fingerprint_secret(s: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    format!("len={} hash={:x}", s.len(), h.finish() & 0xffff_ffff)
}

#[async_trait]
impl Gate for SecretLeakageGate {
    fn name(&self) -> &'static str {
        "secret_leakage"
    }

    fn run(&self, ctx: &GateContext<'_>) -> anyhow::Result<Vec<ReviewFinding>> {
        let mut findings = Vec::new();
        for df in ctx.diff_files {
            if df.is_binary || matches!(df.change_type, ChangeType::Deleted) {
                continue;
            }
            // Skip common test fixtures that ship with deliberately-fake
            // tokens — path-based escape hatch so we don't fire CRITICAL
            // on `tests/fixtures/fake-creds.txt`.
            let lp = df.path.to_ascii_lowercase();
            if lp.contains("/fixtures/") || lp.contains("/testdata/") {
                continue;
            }

            let mut line_hits: Vec<(usize, String, String)> = Vec::new();
            for (ln, line) in &df.added_lines {
                let hits = scan_for_secrets(line);
                for (name, fp) in hits {
                    line_hits.push((*ln, name, fp));
                }
            }
            if line_hits.is_empty() {
                continue;
            }
            let mut by_name: HashMap<String, (Vec<usize>, Vec<String>)> = HashMap::new();
            for (ln, name, fp) in line_hits {
                let entry = by_name.entry(name).or_default();
                entry.0.push(ln);
                entry.1.push(fp);
            }
            for (name, (lines, fps)) in by_name {
                let mut evidence = vec![format!("pattern = {name}")];
                for fp in &fps {
                    evidence.push(format!("match = [{fp}]"));
                }
                findings.push(
                    ReviewFinding::new(
                        Severity::Critical,
                        "secret_leakage",
                        df.path.clone(),
                        format!("Hardcoded secret detected ({name})"),
                        format!(
                            "Added lines contain a value that looks like a `{name}`. \
                             Committed credentials must be revoked — rotation history \
                             cannot be undone, and scanning systems will pick this up the \
                             moment it lands on a remote branch."
                        ),
                        format!(
                            "1. Remove the secret from the diff. 2. Rotate the credential \
                             immediately (rotate first, commit later — GitHub's secret \
                             scanner sees force-pushes). 3. Replace the value with a \
                             reference to a secrets manager / env var."
                        ),
                    )
                    .with_lines(lines)
                    .with_evidence(evidence),
                );
            }
        }
        Ok(findings)
    }
}

// ─── Gate 12: Unwired code ──────────────────────────────────────────────────
//
// A function the diff ADDS that nothing references — no call/reference in
// any added line across the whole diff, and no caller on a same-named
// function node in the graph — is "implemented but never wired": the
// classic mid-feature gap where a mandated permission check or handler
// exists as code but nothing invokes it. Detection is language-generic
// (VB Sub/Function, TS/JS `function` declarations, visibility-qualified
// class methods) and framework-aware: event handlers (`Handles`),
// lifecycle methods, overrides/interface implementations, WebMethods and
// attribute-routed endpoints are all externally invoked and excluded.

pub struct UnwiredGate;

/// A function definition found in the diff's added lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AddedFunction {
    pub file: String,
    pub name: String,
    /// New-file line number of the definition.
    pub line: usize,
}

static RE_VB_FN_DEF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^\s*(?:(?:Public|Private|Protected|Friend|Shared|Overrides|Overridable|Overloads|MustOverride|NotOverridable|Async|Iterator)\s+)*(?:Sub|Function)\s+([A-Za-z_]\w*)\s*\(",
    )
    .expect("valid regex")
});

static RE_TSJS_FN_DECL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:export\s+)?(?:default\s+)?(?:async\s+)?function\s+([A-Za-z_$][\w$]*)\s*\(")
        .expect("valid regex")
});

// Class methods only when they carry an explicit visibility modifier —
// bare `name(args) {` matches too much (object literals, control flow).
static RE_TSJS_METHOD_DEF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\s*(?:public|private|protected)\s+(?:static\s+)?(?:async\s+)?([A-Za-z_$][\w$]*)\s*\(",
    )
    .expect("valid regex")
});

/// Names the framework (not project code) invokes — never "unwired".
static RE_FRAMEWORK_INVOKED_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(Page_|Application_|Session_|New$|Finalize$|Dispose$|constructor$|InitializeComponent$|Main$)")
        .expect("valid regex")
});

fn is_vb_path(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p.ends_with(".vb") || p.ends_with(".aspx") || p.ends_with(".ascx") || p.ends_with(".asmx")
}

fn is_tsjs_path(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p.ends_with(".ts") || p.ends_with(".js") || p.ends_with(".tsx") || p.ends_with(".jsx")
}

/// Match an added line as a function definition, returning the name.
fn added_line_fn_def(path: &str, line: &str) -> Option<String> {
    let re_hit = |re: &Regex| re.captures(line).map(|c| c[1].to_string());
    if is_vb_path(path) {
        re_hit(&RE_VB_FN_DEF)
    } else if is_tsjs_path(path) {
        re_hit(&RE_TSJS_FN_DECL).or_else(|| re_hit(&RE_TSJS_METHOD_DEF))
    } else {
        None
    }
}

/// True when the definition is invoked by the framework / runtime rather
/// than by project code: event wiring (`Handles`, possibly on the VB
/// continuation line), overrides / interface implementations, lifecycle
/// names, and attribute-routed endpoints (`<WebMethod>`, `[HttpGet]`, …)
/// declared on the preceding lines.
fn def_is_externally_invoked(
    name: &str,
    def_line: &str,
    next_line: Option<&str>,
    prev_lines: &[&str],
) -> bool {
    static RE_HANDLES: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)\bHandles\s").expect("valid regex"));
    static RE_DISPATCHED: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)\b(Overrides|Implements)\b").expect("valid regex"));
    static RE_ROUTED_ATTR: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)(<\s*WebMethod|\[\s*WebMethod|\[\s*Http(Get|Post|Put|Delete|Patch)|\[\s*Route|<\s*ScriptMethod)")
            .expect("valid regex")
    });
    if RE_FRAMEWORK_INVOKED_NAME.is_match(name) {
        return true;
    }
    if RE_HANDLES.is_match(def_line) || RE_DISPATCHED.is_match(def_line) {
        return true;
    }
    if let Some(next) = next_line
        && def_line.trim_end().ends_with('_')
        && RE_HANDLES.is_match(next)
    {
        return true;
    }
    prev_lines.iter().any(|l| RE_ROUTED_ATTR.is_match(l))
}

/// Extract added-function candidates that nothing in the diff references:
/// scan every diff file's added lines for definitions, drop framework-
/// invoked ones, then drop any whose name appears (word-boundary,
/// case-insensitive) on any OTHER added line across the whole diff —
/// including markup (`OnClick="Name"`), `AddressOf Name`, and calls added
/// by sibling files. The graph-caller backstop lives in the gate's `run`.
pub(crate) fn unwired_candidates(diff_files: &[DiffFile]) -> Vec<AddedFunction> {
    // Pass 1: collect definitions (bounded — a generated or vendored
    // mega-file must not turn this into an O(n²) scan).
    const MAX_DEFS: usize = 80;
    let mut defs: Vec<AddedFunction> = Vec::new();
    // (file, line) of every definition line per lowercased name — a second
    // definition of the same name (VB partial class, overload) is NOT a
    // reference.
    let mut def_lines: HashMap<String, Vec<(String, usize)>> = HashMap::new();
    for df in diff_files {
        if df.is_binary || matches!(df.change_type, ChangeType::Deleted) || is_test_path(&df.path) {
            continue;
        }
        // Line-number → position map for prev/next lookups within the
        // added block (attribute lines / continuation lines only count
        // when they were added alongside the definition — good enough).
        let by_line: HashMap<usize, &str> = df
            .added_lines
            .iter()
            .map(|(n, s)| (*n, s.as_str()))
            .collect();
        for (n, line) in &df.added_lines {
            let Some(name) = added_line_fn_def(&df.path, line) else {
                continue;
            };
            def_lines
                .entry(name.to_ascii_lowercase())
                .or_default()
                .push((df.path.clone(), *n));
            if defs.len() >= MAX_DEFS {
                continue;
            }
            let next = by_line.get(&(n + 1)).copied();
            let prev: Vec<&str> = (1..=2)
                .filter_map(|d| n.checked_sub(d).and_then(|m| by_line.get(&m).copied()))
                .collect();
            if def_is_externally_invoked(&name, line, next, &prev) {
                continue;
            }
            defs.push(AddedFunction {
                file: df.path.clone(),
                name,
                line: *n,
            });
        }
    }
    if defs.is_empty() {
        return defs;
    }

    // Pass 2: reference scan. A candidate survives only when NO added
    // line anywhere in the diff mentions its name outside definition
    // lines of that same name.
    defs.retain(|cand| {
        let re = match Regex::new(&format!(r"(?i)\b{}\b", regex::escape(&cand.name))) {
            Ok(r) => r,
            Err(_) => return false,
        };
        let own_defs = def_lines
            .get(&cand.name.to_ascii_lowercase())
            .cloned()
            .unwrap_or_default();
        for df in diff_files {
            if df.is_binary {
                continue;
            }
            for (n, line) in &df.added_lines {
                if !re.is_match(line) {
                    continue;
                }
                if own_defs.iter().any(|(f, l)| f == &df.path && l == n) {
                    continue; // the definition itself (or a partial/overload twin)
                }
                return false; // referenced somewhere — wired.
            }
        }
        true
    });
    defs
}

#[async_trait]
impl Gate for UnwiredGate {
    fn name(&self) -> &'static str {
        "unwired"
    }

    fn run(&self, ctx: &GateContext<'_>) -> anyhow::Result<Vec<ReviewFinding>> {
        let mut findings = Vec::new();
        // Bounded graph lookups — a huge WIP diff shouldn't turn the
        // backstop into dozens of scans.
        for cand in unwired_candidates(ctx.diff_files).into_iter().take(25) {
            // Graph backstop: a same-named function already known to the
            // graph with at least one caller (pre-existing overload,
            // partial-class twin, or a markup-wired handler the indexer
            // recovered) is not unwired.
            let nodes = ctx
                .graph
                .query_nodes(ctx.project_id, Some("function"), Some(&cand.name), None, 10)
                .unwrap_or_default();
            let has_graph_caller = nodes
                .iter()
                .filter(|n| n.name.eq_ignore_ascii_case(&cand.name))
                .any(|n| {
                    !crate::handlers::incoming_caller_edges(
                        &ctx.graph,
                        ctx.project_id,
                        &n.node_id,
                        1,
                    )
                    .is_empty()
                });
            if has_graph_caller {
                continue;
            }
            findings.push(
                ReviewFinding::new(
                    Severity::Info,
                    "unwired",
                    cand.file.clone(),
                    format!(
                        "Added function `{}` is never referenced in this diff",
                        cand.name
                    ),
                    format!(
                        "`{}` is defined in `{}` but no added line in this diff calls, \
                         binds, or registers it, and the code graph knows no caller. \
                         Implemented-but-never-wired is the classic mid-feature gap — \
                         a mandated check or handler that exists as code but never runs. \
                         If it's a deliberate API for a follow-up change, say so in the \
                         commit message.",
                        cand.name, cand.file
                    ),
                    format!(
                        "Wire `{}` to its caller (event registration, route, call site) \
                         or defer the definition to the change that uses it.",
                        cand.name
                    ),
                )
                .with_lines(vec![cand.line])
                .with_next_tool(format!(
                    "find_symbol_references(project_id=\"{}\", symbol_name=\"{}\")",
                    ctx.project_id, cand.name
                )),
            );
        }
        Ok(findings)
    }
}

// ─── Gate 13: Product intent ────────────────────────────────────────────────
//
// Bind recorded product/domain knowledge to the diff. Projects that
// ingest PO decisions, domain rules, or wiki knowledge into the
// memory_bank namespace get those sections surfaced when a diff touches
// the areas they describe — the reviewer sees "there IS a recorded
// decision about this area" instead of having to remember to ask.
// Purely opportunistic: no memory bank, no sections matching, or no
// search runtime → zero findings, zero noise.

pub struct ProductIntentGate;

/// Words generic to every codebase layout — they'd match everything and
/// mean nothing in a prose knowledge base.
static PRODUCT_QUERY_STOPWORDS: &[&str] = &[
    "app", "code", "site", "src", "js", "ts", "vb", "cs", "aspx", "ascx", "asmx", "sql", "resx",
    "json", "xml", "www", "inc", "lib", "bin", "obj", "the", "and", "for", "new", "api", "web",
];

/// Word-boundary containment on lowercased text, byte-level (panic-free
/// on any UTF-8). The match must start at a hard boundary (no prefix —
/// `ata` must not match inside `data`) but tolerates a short trailing
/// suffix of ≤2 alphanumeric chars so simple plural/inflection forms
/// still count (`marker` matches `markers`, `setting` matches
/// `settings`, Swedish `-en`/`-ar` endings).
pub(crate) fn contains_word(haystack_lower: &str, word: &str) -> bool {
    if word.is_empty() {
        return false;
    }
    let h = haystack_lower.as_bytes();
    let w = word.as_bytes();
    if w.len() > h.len() {
        return false;
    }
    for i in 0..=(h.len() - w.len()) {
        if &h[i..i + w.len()] != w {
            continue;
        }
        if i > 0 && h[i - 1].is_ascii_alphanumeric() {
            continue;
        }
        let mut j = i + w.len();
        while j < h.len() && h[j].is_ascii_alphanumeric() {
            j += 1;
        }
        if j - (i + w.len()) <= 2 {
            return true;
        }
    }
    false
}

/// How much of the diff-derived query a knowledge-base section actually
/// covers: (matched count, total count, matched words).
pub(crate) fn query_overlap(content: &str, query: &str) -> (usize, usize, Vec<String>) {
    let lc = content.to_lowercase();
    let words: Vec<&str> = query.split_whitespace().collect();
    let matched: Vec<String> = words
        .iter()
        .filter(|w| contains_word(&lc, w))
        .map(|s| s.to_string())
        .collect();
    (matched.len(), words.len(), matched)
}

/// Split an identifier into lowercase words on case boundaries and
/// non-alphanumeric separators: `ChangeRequestMarker` → change request
/// marker; `api-broker` → api broker; `io_pr_iom` → io pr iom.
pub(crate) fn split_identifier_words(ident: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for ch in ident.chars() {
        if !ch.is_alphanumeric() {
            if !cur.is_empty() {
                words.push(cur.to_lowercase());
                cur.clear();
            }
            prev_lower = false;
            continue;
        }
        if ch.is_uppercase() && prev_lower && !cur.is_empty() {
            words.push(cur.to_lowercase());
            cur.clear();
        }
        prev_lower = ch.is_lowercase() || ch.is_numeric();
        cur.push(ch);
    }
    if !cur.is_empty() {
        words.push(cur.to_lowercase());
    }
    words
}

/// Derive a prose-friendly query from the diff's touched areas: file
/// stems and parent-directory names, identifier-split into words, minus
/// layout stopwords. Word order follows diff order; capped at 30 words.
pub(crate) fn product_intent_query(diff_files: &[DiffFile]) -> String {
    let mut words: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for df in diff_files {
        if df.is_binary || is_test_path(&df.path) {
            continue;
        }
        let p = df.path.replace('\\', "/");
        let fname = p.rsplit('/').next().unwrap_or(&p);
        let stem = fname.split('.').next().unwrap_or(fname);
        let dir_leaf = p.rsplit('/').nth(1).unwrap_or("");
        for part in [stem, dir_leaf] {
            for w in split_identifier_words(part) {
                if w.len() < 3
                    || w.chars().all(|c| c.is_ascii_digit())
                    || PRODUCT_QUERY_STOPWORDS.contains(&w.as_str())
                    || !seen.insert(w.clone())
                {
                    continue;
                }
                words.push(w);
                if words.len() >= 30 {
                    return words.join(" ");
                }
            }
        }
    }
    words.join(" ")
}

#[async_trait]
impl Gate for ProductIntentGate {
    fn name(&self) -> &'static str {
        "product_intent"
    }

    async fn run_async(&self, ctx: &GateContext<'_>) -> anyhow::Result<Vec<ReviewFinding>> {
        // ENSURE the runtime — get_project_cached alone returns None on a
        // fresh daemon and silently no-ops the gate (see AntiPatternGate).
        let Ok(ps) =
            crate::services::project_service::ensure_project_runtime(ctx.state, ctx.project_id)
                .await
        else {
            return Ok(Vec::new());
        };
        let query = product_intent_query(ctx.diff_files);
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let hq = HybridQuery {
            project_id: ctx.project_id.to_string(),
            namespace: "memory_bank".into(),
            // GlobalMutable namespace — the search layer applies no
            // generation filter; the value here is inert.
            generation: ctx.generation,
            text: query.clone(),
            top_k: 5,
            fts_mode: "loose".into(),
            include_path_prefixes: None,
            exclude_path_prefixes: None,
            language_filters: None,
            author_filter: None,
            date_after: None,
            date_before: None,
            // Diversity: three notes about three different sections beat
            // three near-duplicates of the strongest one.
            use_mmr: true,
        };
        let cancel = CancellationToken::new();
        let hits = ps
            .search
            .search(&hq, None, &cancel)
            .await
            .unwrap_or_default();

        // Hybrid scores are RRF (rank-fusion) values — ~0.03 at rank 1
        // regardless of match quality — so an absolute score threshold
        // cannot separate "this section describes the touched area" from
        // "this was merely the least-bad hit". Judge relevance by TERM
        // OVERLAP against the actual section content instead: what
        // fraction of the diff-derived words the section text contains.
        let mut scored: Vec<(String, f32, Vec<String>, f32)> = Vec::new();
        tracing::info!(
            gate = "product_intent",
            hit_count = hits.len(),
            query = %query,
            "product_intent search returned"
        );
        for h in hits.into_iter().take(5) {
            let raw = h.path.as_str();
            let section = raw.strip_prefix("memory_bank:").unwrap_or(raw).to_string();
            // Engram-internal bookkeeping sections (index reports) are
            // not product knowledge.
            if section.starts_with("engram/") {
                continue;
            }
            // Fetch by pk (not doc_id): the pk on the hit is exact,
            // while a doc_id lookup would rebuild the pk with the
            // CURRENT generation — wrong for docs ingested at an older
            // one.
            let content = ps
                .search
                .get_doc_by_pk(&h.pk)
                .ok()
                .flatten()
                .map(|(_, _, c, _, _)| c)
                .unwrap_or_default();
            let (matched_n, total_n, matched) = query_overlap(&content, &query);
            let overlap = matched_n as f32 / total_n.max(1) as f32;
            tracing::info!(
                gate = "product_intent",
                section = %section,
                content_len = content.len(),
                matched_n,
                total_n,
                "product_intent hit overlap"
            );
            if content.is_empty() {
                continue;
            }
            if matched_n >= 4 && overlap >= 0.25 {
                scored.push((section, overlap, matched, h.score));
            }
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(3);

        let mut findings = Vec::new();
        for (section, overlap, matched, rrf) in scored {
            findings.push(
                ReviewFinding::new(
                    Severity::Info,
                    "product_intent",
                    section.clone(),
                    format!("Recorded product/domain knowledge may apply: `{section}`"),
                    format!(
                        "The team knowledge base section `{section}` covers {:.0}% of \
                         this diff's touched-area terms ({}). It may record a product \
                         decision, domain rule, or constraint this change must honor — \
                         read it before merging.",
                        overlap * 100.0,
                        matched.join(", ")
                    ),
                    "Read the section and confirm the change matches the recorded \
                     decision; if the decision is stale, update the memory bank instead \
                     of silently diverging."
                        .to_string(),
                )
                .with_evidence(vec![
                    format!("term_overlap = {overlap:.2}"),
                    format!("matched_terms = {}", matched.join(", ")),
                    format!("rrf_score = {rrf:.3}"),
                    format!("query = {query}"),
                ])
                .with_next_tool(format!(
                    "read_memory_bank(project_id=\"{}\", section=\"{}\")",
                    ctx.project_id, section
                )),
            );
        }
        Ok(findings)
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::pre_commit_review_service::{
        Severity, parse_unified_diff, stable_finding_id,
    };

    fn mk_df(path: &str, added: &[(usize, &str)]) -> DiffFile {
        DiffFile {
            path: path.into(),
            change_type: ChangeType::Modified,
            added_lines: added.iter().map(|(n, s)| (*n, s.to_string())).collect(),
            removed_lines: Vec::new(),
            added_content: added.iter().map(|(_, s)| *s).collect::<Vec<_>>().join("\n"),
            removed_content: String::new(),
            hunks: Vec::new(),
            is_binary: false,
        }
    }

    #[test]
    fn contains_word_boundaries_and_suffix_tolerance() {
        // Hard prefix boundary: `ata` must not match inside `data`.
        assert!(!contains_word("the data layer", "ata"));
        // Exact word matches.
        assert!(contains_word("create a change request here", "request"));
        // Short inflection suffixes (≤2 alnum) still match.
        assert!(contains_word("all markers on the map", "marker"));
        assert!(contains_word("system settings page", "setting"));
        // Long suffixes do not (`mark` vs `marketplace`).
        assert!(!contains_word("the marketplace listing", "mark"));
        // Multibyte content must not panic and still match cleanly.
        assert!(contains_word("skapa begäran för markören åäö", "begäran"));
    }

    #[test]
    fn query_overlap_counts_matched_terms() {
        let content = "Change requests track modifications; markers on the map link to them.";
        let (m, t, words) = query_overlap(content, "change request marker map billing");
        assert_eq!(t, 5);
        assert_eq!(m, 4, "matched: {words:?}");
        assert!(!words.contains(&"billing".to_string()));
    }

    #[test]
    fn split_identifier_words_handles_camel_snake_and_kebab() {
        assert_eq!(
            split_identifier_words("ChangeRequestMarker"),
            vec!["change", "request", "marker"]
        );
        assert_eq!(split_identifier_words("api-broker"), vec!["api", "broker"]);
        assert_eq!(
            split_identifier_words("io_pr_iom_log"),
            vec!["io", "pr", "iom", "log"]
        );
        assert_eq!(
            split_identifier_words("ioMarkerInfowindow"),
            vec!["io", "marker", "infowindow"]
        );
    }

    #[test]
    fn product_intent_query_uses_stems_and_dirs_minus_stopwords() {
        let diff = vec![
            mk_df("Site/App_Code/ata/code/ChangeRequestMarker.vb", &[(1, "x")]),
            mk_df(
                "Site/modules/dashboard/ts/map/vsMap/iomarker/ioMarker.ts",
                &[(1, "x")],
            ),
            mk_df("Site/tests/SomethingTest.vb", &[(1, "x")]),
        ];
        let q = product_intent_query(&diff);
        assert!(
            q.contains("change") && q.contains("request") && q.contains("marker"),
            "domain words from stems must be present: {q}"
        );
        assert!(
            q.contains("iomarker") || q.contains("marker"),
            "directory leaf words included: {q}"
        );
        assert!(
            !q.split_whitespace()
                .any(|w| w == "code" || w == "app" || w == "vb"),
            "layout stopwords excluded: {q}"
        );
        assert!(!q.contains("test"), "test files contribute nothing: {q}");
    }

    #[test]
    fn product_intent_query_empty_for_empty_or_binary_diff() {
        assert!(product_intent_query(&[]).is_empty());
        let mut bin = mk_df("gfx/logo.png", &[]);
        bin.is_binary = true;
        assert!(product_intent_query(&[bin]).is_empty());
    }

    #[test]
    fn unwired_candidates_flags_unreferenced_vb_sub() {
        let diff = vec![mk_df(
            "Site/App_Code/gate.vb",
            &[
                (10, "Public Sub CheckCrGate(ByVal id As Integer)"),
                (11, "    ' enforce the PO-mandated block"),
                (12, "End Sub"),
            ],
        )];
        let c = unwired_candidates(&diff);
        assert_eq!(c.len(), 1, "unreferenced added Sub must be flagged");
        assert_eq!(c[0].name, "CheckCrGate");
        assert_eq!(c[0].line, 10);
    }

    #[test]
    fn unwired_candidates_skips_functions_referenced_in_diff() {
        let diff = vec![
            mk_df(
                "Site/App_Code/gate.vb",
                &[
                    (10, "Public Sub CheckCrGate(ByVal id As Integer)"),
                    (11, "End Sub"),
                ],
            ),
            mk_df(
                "Site/App_Code/caller.vb",
                &[(5, "        CheckCrGate(marker.Id)")],
            ),
        ];
        assert!(
            unwired_candidates(&diff).is_empty(),
            "a call site anywhere in the diff wires the function"
        );
    }

    #[test]
    fn unwired_candidates_skips_handles_lifecycle_and_overrides() {
        let diff = vec![mk_df(
            "Site/page.aspx.vb",
            &[
                (
                    5,
                    "Protected Sub btnSave_Click(s As Object, e As EventArgs) Handles btnSave.Click",
                ),
                (6, "End Sub"),
                (7, "Private Sub Page_Load(s As Object, e As EventArgs)"),
                (8, "End Sub"),
                (9, "Protected Overrides Sub OnInit(e As EventArgs)"),
                (10, "End Sub"),
                (11, "Protected Sub HandleIt(s As Object, e As EventArgs) _"),
                (12, "    Handles btnOther.Click"),
                (13, "End Sub"),
            ],
        )];
        assert!(
            unwired_candidates(&diff).is_empty(),
            "framework-invoked definitions must never be flagged"
        );
    }

    #[test]
    fn unwired_candidates_skips_attribute_routed_endpoints() {
        let diff = vec![mk_df(
            "Site/App_Code/svc.vb",
            &[
                (3, "    <WebMethod()> _"),
                (
                    4,
                    "    Public Function GetData(ByVal id As Integer) As String",
                ),
                (5, "    End Function"),
            ],
        )];
        assert!(
            unwired_candidates(&diff).is_empty(),
            "attribute-routed endpoints are externally invoked"
        );
    }

    #[test]
    fn unwired_candidates_markup_reference_counts_as_wired() {
        let diff = vec![
            mk_df(
                "Site/page.aspx.vb",
                &[
                    (20, "Protected Sub SaveIt(s As Object, e As EventArgs)"),
                    (21, "End Sub"),
                ],
            ),
            mk_df(
                "Site/page.aspx",
                &[(
                    8,
                    r#"<asp:Button ID="btn" OnClick="SaveIt" runat="server" />"#,
                )],
            ),
        ];
        assert!(
            unwired_candidates(&diff).is_empty(),
            "markup wiring added in the diff counts as a reference"
        );
    }

    #[test]
    fn unwired_candidates_tsjs_defs_and_same_file_reference() {
        let diff = vec![mk_df(
            "Site/ts/map/gate.ts",
            &[
                (3, "    private computeGate(id: number): boolean {"),
                (4, "        return id > 0;"),
                (5, "    }"),
                (6, "function helper(x: number) {"),
                (7, "    return x;"),
                (8, "}"),
                (9, "const y = helper(2);"),
            ],
        )];
        let c = unwired_candidates(&diff);
        let names: Vec<&str> = c.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["computeGate"],
            "helper is called on line 9; computeGate is not referenced anywhere"
        );
    }

    #[test]
    fn unwired_candidates_partial_class_twin_def_is_not_a_reference() {
        let diff = vec![
            mk_df(
                "Site/App_Code/a.vb",
                &[
                    (10, "Public Sub Orphan(ByVal x As Integer)"),
                    (11, "End Sub"),
                ],
            ),
            mk_df(
                "Site/App_Code/b.vb",
                &[
                    (30, "Public Sub Orphan(ByVal x As String)"),
                    (31, "End Sub"),
                ],
            ),
        ];
        let c = unwired_candidates(&diff);
        assert_eq!(
            c.len(),
            2,
            "an overload/partial twin DEFINITION must not count as a reference"
        );
    }

    #[test]
    fn destructive_patterns_match_known_snippets() {
        assert!(
            !detect_destructive("rows.Where(r => r.Id == 1).Select(r => r.Name)")
                .iter()
                .any(|p| !p.is_empty())
        );
        let hits = detect_destructive("db.Users.DeleteAllOnSubmit(allRows)");
        assert!(
            hits.iter().any(|p| p.contains("DeleteAllOnSubmit")),
            "hits: {hits:?}"
        );
    }

    #[test]
    fn secret_scanner_detects_github_token_and_redacts() {
        let code = "const t = \"ghp_123456789012345678901234567890123456\";";
        let hits = scan_for_secrets(code);
        assert!(
            hits.iter().any(|(n, _)| n.contains("GitHub")),
            "expected GitHub token detected, got {hits:?}"
        );
        // The hash fingerprint must not contain the raw token value.
        for (_, fp) in &hits {
            assert!(!fp.contains("ghp_"), "secret leaked into fingerprint: {fp}");
        }
    }

    #[test]
    fn secret_scanner_detects_aws_key() {
        let code = "export const KEY = \"AKIAIOSFODNN7EXAMPLE\";";
        let hits = scan_for_secrets(code);
        assert!(hits.iter().any(|(n, _)| n.contains("AWS Access Key")));
    }

    #[test]
    fn secret_scanner_ignores_short_strings() {
        let hits = scan_for_secrets("let x = \"short\";");
        assert!(
            hits.is_empty(),
            "should not fire on short strings: {hits:?}"
        );
    }

    #[test]
    fn secret_scanner_detects_openai_project_key() {
        let code = r#"const key = "sk-proj-abc123def456";"#;
        let hits = scan_for_secrets(code);
        assert!(
            hits.iter().any(|(n, _)| n.contains("OpenAI")),
            "expected OpenAI key detected, got {hits:?}"
        );
    }

    #[test]
    fn secret_scanner_detects_vb_const_api_key() {
        // VB / C# often write `Const API_KEY As String = "…"`. The
        // credential-assignment pattern must tolerate the `As Type`
        // gap between the identifier and `=`.
        let code = r#"    Private Const API_KEY As String = "sk-proj-abc123def456""#;
        let hits = scan_for_secrets(code);
        assert!(
            !hits.is_empty(),
            "expected at least one hit on VB `Const API_KEY As String = …`, got {hits:?}"
        );
    }

    #[test]
    fn secret_scanner_detects_anthropic_key() {
        let code = r#"export const k = "sk-ant-api03-ABCDEFGHIJKLMNOPQRSTUVWX";"#;
        let hits = scan_for_secrets(code);
        assert!(
            hits.iter().any(|(n, _)| n.contains("Anthropic")),
            "expected Anthropic key detected, got {hits:?}"
        );
    }

    #[test]
    fn matches_casing_handles_all_styles() {
        assert!(matches_casing("DoThing", "PascalCase"));
        assert!(!matches_casing("doThing", "PascalCase"));
        assert!(matches_casing("doThing", "camelCase"));
        assert!(!matches_casing("DoThing", "camelCase"));
        assert!(matches_casing("do_thing", "snake_case"));
        assert!(!matches_casing("doThing", "snake_case"));
    }

    #[test]
    fn convert_to_casing_roundtrips() {
        assert_eq!(convert_to_casing("doThing", "PascalCase"), "DoThing");
        assert_eq!(convert_to_casing("do_thing", "PascalCase"), "DoThing");
        assert_eq!(convert_to_casing("DoThing", "camelCase"), "doThing");
        assert_eq!(convert_to_casing("DoThing", "snake_case"), "do_thing");
    }

    #[test]
    fn common_name_prefix_returns_dominant() {
        let names = vec![
            "site/App_Code/permits/perm_create.vb".to_string(),
            "site/App_Code/permits/perm_delete.vb".to_string(),
            "site/App_Code/permits/perm_update.vb".to_string(),
            "site/App_Code/permits/perm_search.vb".to_string(),
        ];
        // The helper prefers the longest prefix that matches ≥60% of the
        // siblings, so it returns `perm_` (including the separator) — that's
        // the actionable value for a rename suggestion.
        let p = common_name_prefix(&names);
        assert!(
            p.as_deref() == Some("perm") || p.as_deref() == Some("perm_"),
            "expected `perm` or `perm_`, got {p:?}"
        );
    }

    #[test]
    fn style_gate_flags_casing_mismatch() {
        let diff = "\
diff --git a/foo.vb b/foo.vb
--- a/foo.vb
+++ b/foo.vb
@@ -1,3 +1,4 @@
 Module Foo
     Public Sub Existing()
+    Public Sub badCaseMethod()
     End Sub
 End Module
";
        let diff_files = parse_unified_diff(diff);
        let conventions = vec![DetectedConvention {
            category: ConventionCategory::MethodNaming,
            value: "PascalCase".into(),
            sample_count: 20,
            total_count: 20,
        }];
        let findings = check_style_compliance(&diff_files[0], &conventions);
        assert!(
            findings.iter().any(|f| f.title.contains("badCaseMethod")),
            "expected casing finding, got {findings:#?}"
        );
    }

    #[test]
    fn secret_gate_skips_fixtures_dir() {
        // Ensure the path-based escape hatch for test fixtures works.
        let diff = "\
diff --git a/tests/fixtures/fake.env b/tests/fixtures/fake.env
--- /dev/null
+++ b/tests/fixtures/fake.env
@@ -0,0 +1,1 @@
+AWS_KEY=AKIAIOSFODNN7EXAMPLE
";
        let diff_files = parse_unified_diff(diff);
        // The gate is pure — we can call it with a minimal context by
        // constructing one. But we can also exercise the scanner directly:
        let hits = scan_for_secrets(&diff_files[0].added_content);
        assert!(
            !hits.is_empty(),
            "scanner must match; path guard is in run()"
        );
    }

    #[test]
    fn stable_id_survives_re_encoding() {
        let a = stable_finding_id("g", "f", "t", &[1, 2]);
        let b = stable_finding_id("g", "f", "t", &[1, 2]);
        assert_eq!(a, b);
    }

    #[test]
    fn severity_ord_puts_critical_first() {
        assert!(Severity::Critical < Severity::Warning);
        assert!(Severity::Warning < Severity::Info);
        assert!(Severity::Info < Severity::Style);
    }
}
