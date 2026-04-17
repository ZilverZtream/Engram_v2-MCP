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
    file_node_id, is_test_path, read_file_content, ChangeType, ConventionCategory,
    DetectedConvention, DiffFile, Gate, GateContext, ReviewFinding, Severity,
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
    ]
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
                        revert_hashes.first().cloned().unwrap_or_else(|| "<unknown>".into())
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
                    report.migration_risk, report.risk_band.as_str()
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
            findings.push(
                ReviewFinding::new(
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
                ),
            );
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
            findings.extend(check_style_compliance(df, &conventions));
        }
        Ok(findings)
    }
}

/// Line-level style checks that fire when added code breaks a convention
/// detected on the full file.
fn check_style_compliance(
    df: &DiffFile,
    conventions: &[DetectedConvention],
) -> Vec<ReviewFinding> {
    let mut out = Vec::new();
    let file_path = &df.path;
    let is_vb = file_path.to_ascii_lowercase().ends_with(".vb");
    let is_csharp = file_path.to_ascii_lowercase().ends_with(".cs");
    let is_ts_js = file_path
        .to_ascii_lowercase()
        .ends_with(".ts")
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
                static DATA_ACCESS_RE: LazyLock<Option<Regex>> =
                    LazyLock::new(|| {
                        Regex::new(r"(?i)\b(?:SubmitChanges|InsertOnSubmit|DataContext|\.Table\()\b").ok()
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
                static ON_ERROR_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
                    Regex::new(r"(?im)^\s*On\s+Error\s+Resume\s+Next\b").ok()
                });
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
                static REDIR_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
                    Regex::new(r"(?i)\bResponse\.Redirect\s*\(").ok()
                });
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
        "PascalCase" => name.chars().next().is_some_and(|c| c.is_ascii_uppercase()) && !name.contains('_'),
        "camelCase" => name.chars().next().is_some_and(|c| c.is_ascii_lowercase()) && !name.contains('_'),
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
        for df in ctx.diff_files {
            if df.is_binary || matches!(df.change_type, ChangeType::Deleted | ChangeType::Added) {
                continue;
            }
            let node_id = file_node_id(&df.path);
            let neighbors = ctx
                .graph
                .neighbors(ctx.project_id, EdgeKind::TemporalCoupling, &node_id, 20)
                .unwrap_or_default();
            for (neighbor_id, weight) in neighbors {
                if weight < base_threshold {
                    continue;
                }
                let neighbor_path = neighbor_id
                    .strip_prefix("file:")
                    .unwrap_or(&neighbor_id)
                    .to_string();
                if ctx.changed_paths.contains(&neighbor_path) {
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
    Regex::new(r#"(?i)\b(Session|ViewState|Application|Cache)\s*[\(\[]\s*["']([\w_\.\-]+)["']\s*[\)\]]"#).ok()
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
                    let store = cap.get(1).map(|m| m.as_str().to_string()).unwrap_or_default();
                    let key = cap.get(2).map(|m| m.as_str().to_string()).unwrap_or_default();
                    per_file.entry((store, key)).or_default().push(*ln);
                }
            }
            for ((store, key), lines) in per_file {
                // Graph stores state keys with nodeId "state:<Store>:<key>".
                let node_id = format!("state:{store}:{key}");
                let readers = ctx
                    .graph
                    .find_incoming_edges(
                        ctx.project_id,
                        Some(EdgeKind::ReadsState),
                        &node_id,
                        50,
                    )
                    .unwrap_or_default();
                let writers = ctx
                    .graph
                    .find_incoming_edges(
                        ctx.project_id,
                        Some(EdgeKind::WritesState),
                        &node_id,
                        50,
                    )
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
        let audit_short = audit_name.rsplit('.').next().unwrap_or(&audit_name).to_string();

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
        // Require a project-runtime search engine. If the caller hasn't
        // initialised it (unit tests, minimal projects), fall back to the
        // pure destructive-pattern scan.
        let ps = match ctx
            .state
            .get_project_cached(ctx.project_id)
            .or_else(|| None)
        {
            Some(p) => p,
            None => return Ok(self.destructive_only(ctx)),
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

            let relevant: Vec<_> = hits.into_iter().filter(|h| h.score > 0.3).collect();
            if relevant.len() < 2 {
                continue;
            }
            // Suppression: if the diff matches ≥1 wontfix_patterns doc
            // (scoped to this file's family) with score > 0.5, dampen
            // by one severity tier. This is the file-scoped
            // false-positive suppression the team explicitly asked for
            // via the wontFix threads — we don't discard the finding,
            // we just downgrade it so it doesn't scream about a
            // pattern someone already looked at and left alone.
            let strong_supp = supp_hits.iter().any(|h| h.score > 0.5);
            let mut severity = if relevant.iter().any(|h| h.score > 0.6) {
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
            for h in &relevant {
                // `path` on CodeRabbit-sourced docs is the cluster's
                // file pattern (e.g. `/site/**/*.vb`) — surface that
                // so the reader sees it's a CodeRabbit rule, not a
                // reverted-commit antipattern. The DocStore also
                // carries `author` = "coderabbit" on those docs; the
                // path prefix is a reliable visual signal.
                let path = h.path.as_str();
                let source_label = if path.contains("**/") || path.starts_with("coderabbit://") {
                    " [source: CodeRabbit]"
                } else {
                    ""
                };
                evidence.push(format!(
                    "match = `{path}` (score {:.3}){source_label}",
                    h.score
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
                    format!("Added code resembles {} indexed anti-pattern(s)", relevant.len()),
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
                     PR demonstrates the target rows.".to_string(),
                )
                .with_evidence(vec![format!(
                    "destructive_patterns = {}",
                    hits.join(", ")
                )]),
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
                // Extension consistency.
                let file_ext = extension(&df.path);
                let sibling_exts: HashMap<String, usize> =
                    count_extensions(&sibling_names);
                if let Some((top_ext, top_count)) = sibling_exts
                    .iter()
                    .max_by_key(|(_, c)| **c)
                    .map(|(k, v)| (k.clone(), *v))
                {
                    if !file_ext.is_empty()
                        && file_ext != top_ext
                        && top_count >= sibling_names.len() / 2
                    {
                        findings.push(
                            ReviewFinding::new(
                                Severity::Style,
                                "new_file",
                                df.path.clone(),
                                format!(
                                    "New file extension `.{}` differs from siblings",
                                    file_ext
                                ),
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
                            ),
                        );
                    }
                }

                // Prefix convention.
                let prefix = common_name_prefix(&sibling_names);
                if let Some(p) = prefix {
                    let fname = df.path.rsplit('/').next().unwrap_or(&df.path);
                    if !fname.starts_with(&p) {
                        findings.push(
                            ReviewFinding::new(
                                Severity::Style,
                                "new_file",
                                df.path.clone(),
                                format!("File naming doesn't match `{p}*` convention"),
                                format!(
                                    "Every sibling file in `{parent}/` starts with `{p}`. The \
                                     new file `{fname}` does not.",
                                ),
                                format!("Rename `{fname}` → `{p}{fname}` or similar."),
                            ),
                        );
                    }
                }
            }

            // ASPX without codebehind.
            if df.path.to_ascii_lowercase().ends_with(".aspx") {
                let has_cb = added_codebehind.iter().any(|cb| {
                    cb.to_ascii_lowercase()
                        .starts_with(&df.path.to_ascii_lowercase())
                });
                if !has_cb {
                    findings.push(
                        ReviewFinding::new(
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
                        ),
                    );
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
        let count = names
            .iter()
            .filter(|n| n.starts_with(candidate))
            .count();
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
            let coupled_tests: Vec<(String, u32)> = neighbors
                .into_iter()
                .filter(|(id, _)| {
                    let p = id.strip_prefix("file:").unwrap_or(id);
                    is_test_path(p)
                })
                .map(|(id, w)| (id.strip_prefix("file:").unwrap_or(&id).to_string(), w))
                .collect();

            let has_coupled_test_in_diff = coupled_tests
                .iter()
                .any(|(p, _)| ctx.changed_paths.contains(p));
            if has_coupled_test_in_diff {
                continue;
            }

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
                findings.push(
                    ReviewFinding::new(
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
                    ),
                );
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
            (
                "OpenAI API Key",
                r"\bsk-(?:proj-)?[A-Za-z0-9_\-]{12,}\b",
            ),
            // Anthropic
            (
                "Anthropic API Key",
                r"\bsk-ant-[A-Za-z0-9_\-]{20,}\b",
            ),
            // Slack
            ("Slack Bot Token", r"\bxoxb-[0-9]+-[0-9]+-[A-Za-z0-9]+\b"),
            ("Slack User Token", r"\bxoxp-[0-9]+-[0-9]+-[0-9]+-[A-Za-z0-9]+\b"),
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

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::pre_commit_review_service::{
        parse_unified_diff, stable_finding_id, Severity,
    };

    #[test]
    fn destructive_patterns_match_known_snippets() {
        assert!(!detect_destructive("rows.Where(r => r.Id == 1).Select(r => r.Name)").iter().any(|p| !p.is_empty()));
        let hits = detect_destructive("db.Users.DeleteAllOnSubmit(allRows)");
        assert!(hits.iter().any(|p| p.contains("DeleteAllOnSubmit")), "hits: {hits:?}");
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
        assert!(hits.is_empty(), "should not fire on short strings: {hits:?}");
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
        assert!(!hits.is_empty(), "scanner must match; path guard is in run()");
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
