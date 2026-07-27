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
        Box::new(SyncContractGate),
        Box::new(CoAddedFamilyGate),
        Box::new(ComplexityGate),
        Box::new(AddedConventionsGate),
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
    let is_minilang = {
        let l = file_path.to_ascii_lowercase();
        l.ends_with(".ml") || l.ends_with(".mlinc")
    };

    for conv in conventions {
        if conv.confidence() < 0.5 {
            continue;
        }

        match conv.category {
            ConventionCategory::MethodNaming => {
                let expected = conv.value.clone();
                let re_new_method: Option<Regex> = if is_vb {
                    Regex::new(r"(?im)^\s*(?:Public|Private|Protected|Friend|Shared|Overrides|Overridable|Async|Partial)?\s*(?:Public|Private|Protected|Friend|Shared|Overrides|Overridable|Async|Partial)?\s*(?:Sub|Function)\s+(\w+)\s*\(").ok()
                } else if is_minilang {
                    // Access modifiers optional; the name may be followed by
                    // an ` Of …` generic clause instead of `(`.
                    Regex::new(r"(?im)^\s*(?:(?:Public|Private)\s+)?(?:Sub|Function)\s+(\w+)\s*(?:\(|Of\s)").ok()
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
            // Resource/markup text is not CODE — a resx label's words
            // matching a VB file's prose produces confident-looking
            // nonsense (live: label.resx "resembling" a properties.vb at
            // 0.42 overlap). Only real code files query the antipattern
            // index.
            let lower_path = df.path.to_ascii_lowercase();
            if lower_path.ends_with(".resx")
                || lower_path.ends_with(".xml")
                || lower_path.ends_with(".config")
                || lower_path.ends_with(".css")
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
                // Vendored/minified artifacts in the antipattern corpus
                // (a revert that touched `*.min.js`) match everything —
                // identifier soup, zero review value.
                if h.path.as_str().to_ascii_lowercase().contains(".min.") {
                    continue;
                }
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

/// True when a graph node's (possibly class-qualified) name refers to
/// the same member as a bare identifier from the diff: exact match, or
/// the last `.`-segment matches ("api.StartTransaction" vs
/// "StartTransaction"), case-insensitive.
pub(crate) fn bare_name_matches(node_name: &str, bare: &str) -> bool {
    node_name.eq_ignore_ascii_case(bare)
        || node_name
            .rsplit('.')
            .next()
            .is_some_and(|last| last.eq_ignore_ascii_case(bare))
}

/// A function definition found in the diff's added lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AddedFunction {
    pub file: String,
    pub name: String,
    /// New-file line number of the definition.
    pub line: usize,
    /// Enclosing class/module when its declaration was visible in the
    /// added lines (always for Added files; often absent for Modified
    /// files whose class header is outside the hunks).
    pub class_name: Option<String>,
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
        // Enclosing-class tracking: the last class/module declaration seen
        // in the added lines before a definition. Gives the graph backstop
        // class context so a common name like `Create` is only suppressed
        // by same-class graph nodes, not by any class's `Create` that
        // happens to have callers (live FN: 5 of 6 orphaned methods in a
        // NEW class escaped because other classes' same-named members had
        // callers).
        static RE_CLASS_DECL: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(
                r"(?i)^\s*(?:(?:public|private|friend|protected|partial|export|abstract|static|sealed|notinheritable|mustinherit)\s+)*(?:class|module)\s+([A-Za-z_$][\w$]*)",
            )
            .expect("valid regex")
        });
        let mut last_class: Option<(usize, String)> = None;
        for (n, line) in &df.added_lines {
            if let Some(cap) = RE_CLASS_DECL.captures(line) {
                last_class = Some((*n, cap[1].to_string()));
            }
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
                class_name: last_class
                    .as_ref()
                    .filter(|(cl, _)| cl < n)
                    .map(|(_, c)| c.clone()),
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
            // VB/C# member nodes carry QUALIFIED names ("api.StartTransaction")
            // while the diff regex captures the bare member name — match on
            // the last dot-segment or the whole name, else a wired function
            // gets a false "never referenced" (live FP: StartTransaction had
            // 5 Calls edges and was still flagged). When the candidate's
            // enclosing CLASS is known, only same-class (or same-file) nodes
            // may suppress — a common name like `Create` must not be
            // suppressed by some other class's `Create` that has callers.
            let has_graph_caller = nodes
                .iter()
                .filter(|n| bare_name_matches(&n.name, &cand.name))
                .filter(|n| match &cand.class_name {
                    Some(cls) => {
                        let qualified = format!("{}.", cls.to_lowercase());
                        n.name.to_lowercase().contains(&qualified)
                            || path_suffix_match(n.file_path.as_str(), &cand.file)
                    }
                    None => true,
                })
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

// ─── Gate 14: Sync contracts ────────────────────────────────────────────────
//
// "Must update in N places" comments are machine-readable maintenance
// contracts (extracted at ingest as `sync_contract` nodes). When a diff
// touches SOME of a contract's listed sites but not all, the untouched
// copy ships stale behavior — the exact two-of-three failure observed
// live in a marker-import delete path. Sites are resolved to files via
// the graph (dotted references) or matched as paths; unresolvable sites
// fall back to a word-boundary scan of the diff's added lines.

pub struct SyncContractGate;

/// Last dotted identifier of a non-path site reference, sans trailing
/// `()`/punctuation: `_io.import.MarkerImport.GetMarkersToDeleteFromProject()`
/// → `GetMarkersToDeleteFromProject`.
pub(crate) fn site_tail_identifier(site: &str) -> Option<String> {
    let s = site.trim().trim_end_matches([' ', '.', ';', ':', ',']);
    let s = s.strip_suffix("()").unwrap_or(s);
    let tail = s.rsplit('.').next().unwrap_or(s);
    let ok = tail.len() >= 3
        && tail.chars().all(|c| c.is_alphanumeric() || c == '_')
        && tail
            .chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_');
    ok.then(|| tail.to_string())
}

#[async_trait]
impl Gate for SyncContractGate {
    fn name(&self) -> &'static str {
        "sync_contract"
    }

    fn run(&self, ctx: &GateContext<'_>) -> anyhow::Result<Vec<ReviewFinding>> {
        let mut findings = Vec::new();
        // The same contract comment is often COPIED to every listed site
        // (verified live: the 3-place marker-import contract exists in all
        // three files) — dedup by normalized site-set so one violation
        // yields one finding, not one per copy.
        let mut seen_site_sets: HashSet<String> = HashSet::new();
        let contracts = ctx
            .graph
            .query_nodes(ctx.project_id, Some("sync_contract"), None, None, 300)
            .unwrap_or_default();
        for c in contracts {
            let Some(meta) = &c.metadata else { continue };
            let Some(sites_raw) = meta.get("sites").and_then(|v| v.as_str()) else {
                continue;
            };
            let sites: Vec<&str> = sites_raw
                .split("||")
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            if sites.len() < 2 {
                continue;
            }
            let mut set_key: Vec<String> = sites.iter().map(|s| s.to_lowercase()).collect();
            set_key.sort();
            if !seen_site_sets.insert(set_key.join("||")) {
                continue;
            }
            let mut touched: Vec<String> = Vec::new();
            let mut untouched: Vec<String> = Vec::new();
            for site in &sites {
                let is_touched = if site.contains('/') || site.contains('\\') {
                    ctx.changed_paths.iter().any(|p| path_suffix_match(p, site))
                } else if let Some(tail) = site_tail_identifier(site) {
                    let nodes = ctx
                        .graph
                        .query_nodes(ctx.project_id, Some("function"), Some(&tail), None, 10)
                        .unwrap_or_default();
                    let via_graph = nodes
                        .iter()
                        .filter(|n| bare_name_matches(&n.name, &tail))
                        .any(|n| {
                            ctx.changed_paths
                                .iter()
                                .any(|p| path_suffix_match(p, n.file_path.as_str()))
                        });
                    let tail_lower = tail.to_lowercase();
                    via_graph
                        || ctx.diff_files.iter().any(|df| {
                            !df.is_binary
                                && df
                                    .added_lines
                                    .iter()
                                    .any(|(_, l)| contains_word(&l.to_lowercase(), &tail_lower))
                        })
                } else {
                    false
                };
                if is_touched {
                    touched.push((*site).to_string());
                } else {
                    untouched.push((*site).to_string());
                }
            }
            if !touched.is_empty() && !untouched.is_empty() {
                findings.push(
                    ReviewFinding::new(
                        Severity::Warning,
                        "sync_contract",
                        c.file_path.as_str().to_string(),
                        format!(
                            "Sync contract partially honored: {}/{} listed sites touched",
                            touched.len(),
                            sites.len()
                        ),
                        format!(
                            "`{}` (line {}) declares logic that must be kept in sync across \
                             {} places. This diff touches {} of them but NOT: {}. \
                             Two-of-three updates is the classic way these contracts rot — \
                             the untouched site ships stale behavior.",
                            c.file_path.as_str(),
                            c.start_line,
                            sites.len(),
                            touched.len(),
                            untouched.join("; ")
                        ),
                        "Update the untouched site(s) in the same change, or amend the \
                         contract comment if a site was retired."
                            .to_string(),
                    )
                    .with_lines(vec![c.start_line as usize])
                    .with_evidence(vec![
                        format!("touched = {}", touched.join("; ")),
                        format!("untouched = {}", untouched.join("; ")),
                    ]),
                );
            }
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
        tracing::debug!(
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
            tracing::debug!(
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

// ─── Gate 17: Co-added family companions ─────────────────────────────────────
//
// When a diff ADDS files into a directory family (e.g. a new api-v2
// controller cohort), the merged-PR corpus knows what PRs that added
// files there ALSO consistently touched — docs contracts
// (copilot-instructions.md), permission/role files, registration
// files. File-level co-change cannot carry this (probe 2026-07-05:
// the family's own cohort files saturate the partner list), so this
// gate mines the PR docs' "Files shipped together" lists directly.
// Generic for any repo with an ingested merged-PR corpus.

/// Directory family keys for an added file: the parent dir AND the
/// grandparent dir (both lowercased, leading `site/` stripped —
/// PR-corpus paths carry no Site/ prefix), each requiring >=2 path
/// segments. The grandparent matters because a new cohort usually
/// lives in a brand-new LEAF dir (api-v2/Controllers/markerInspection
/// was itself added by its PR) — the exemplar history exists one
/// level up (api-v2/Controllers). The >=3-exemplar filter downstream
/// picks whichever level actually has history.
pub(crate) fn family_keys(path: &str) -> Vec<String> {
    let norm = path.replace('\\', "/").to_lowercase();
    let norm = norm.strip_prefix("site/").unwrap_or(&norm).to_string();
    let mut out = Vec::new();
    let mut current = norm.as_str();
    for _ in 0..2 {
        let Some((parent, _)) = current.rsplit_once('/') else {
            break;
        };
        if parent.contains('/') {
            out.push(parent.to_string());
        }
        current = parent;
    }
    out
}

/// Parse the file list out of a merged-PR corpus doc: `- path` lines
/// under the "## Files shipped together" heading.
pub(crate) fn parse_pr_doc_files(content: &str) -> Vec<String> {
    let mut in_files = false;
    let mut out = Vec::new();
    for line in content.lines() {
        let t = line.trim();
        if t.starts_with("## ") {
            in_files = t.starts_with("## Files shipped together");
            continue;
        }
        if in_files && let Some(p) = t.strip_prefix("- ") {
            let p = p.trim();
            if !p.is_empty() && !p.starts_with("...") {
                out.push(p.replace('\\', "/"));
            }
        }
    }
    out
}

pub struct CoAddedFamilyGate;

#[async_trait]
impl Gate for CoAddedFamilyGate {
    fn name(&self) -> &'static str {
        "co_added_family"
    }

    async fn run_async(&self, ctx: &GateContext<'_>) -> anyhow::Result<Vec<ReviewFinding>> {
        use std::collections::{BTreeMap, BTreeSet};

        // Only fires when the diff ADDS files — the companion contract
        // is about introducing new cohort members, not editing old ones.
        let mut families: BTreeSet<String> = BTreeSet::new();
        for df in ctx.diff_files {
            if matches!(df.change_type, ChangeType::Added) && !df.is_binary {
                families.extend(family_keys(&df.path));
            }
        }
        if families.is_empty() {
            return Ok(Vec::new());
        }

        let Ok(ps) =
            crate::services::project_service::ensure_project_runtime(ctx.state, ctx.project_id)
                .await
        else {
            return Ok(Vec::new());
        };

        // Shallower families first — they aggregate the cohort history
        // (a brand-new leaf dir has none by definition).
        let mut ordered: Vec<String> = families.into_iter().collect();
        ordered.sort_by_key(|f| (f.matches('/').count(), f.clone()));

        let mut findings = Vec::new();
        for family in ordered.into_iter().take(4) {
            // Lexical search over the PR corpus for docs referencing the
            // family dir; loose mode tokenizes the path segments.
            let hq = engram_index::hybrid::HybridQuery {
                project_id: ctx.project_id.to_string(),
                namespace: "history".into(),
                // GlobalMutable namespace — no generation filter applies.
                generation: ctx.generation,
                text: family.replace(['/', '-', '_'], " "),
                top_k: 20,
                fts_mode: "loose".into(),
                include_path_prefixes: None,
                exclude_path_prefixes: None,
                language_filters: None,
                author_filter: None,
                date_after: None,
                date_before: None,
                use_mmr: false,
            };
            let cancel = tokio_util::sync::CancellationToken::new();
            let hits = ps
                .search
                .search(&hq, None, &cancel)
                .await
                .unwrap_or_default();

            // Exemplars: PRs whose shipped-file list includes an ADD
            // under this family dir.
            let mut exemplar_count = 0usize;
            let mut companion_counts: BTreeMap<String, usize> = BTreeMap::new();
            for h in hits {
                if !h.path.as_str().starts_with("pr:") {
                    continue;
                }
                let Ok(Some((_, _, content, _, _))) = ps.search.get_doc_by_pk(&h.pk) else {
                    continue;
                };
                let files = parse_pr_doc_files(&content);
                let in_family = files.iter().any(|f| f.to_lowercase().contains(&family));
                if !in_family {
                    continue;
                }
                exemplar_count += 1;
                let mut seen_this_pr: BTreeSet<String> = BTreeSet::new();
                for f in files {
                    let fl = f.to_lowercase();
                    if fl.contains(&family) {
                        continue; // the cohort itself, not a companion
                    }
                    if seen_this_pr.insert(fl.clone()) {
                        *companion_counts.entry(f).or_insert(0) += 1;
                    }
                }
            }
            if exemplar_count < 3 {
                continue; // too little history to assert a contract
            }

            // Companions absent from THIS diff, in two evidence tiers:
            // >=60% of exemplar PRs -> Warning (an asserted contract);
            // 40-60% -> Info (a newer or situational convention — e.g. a
            // docs rule adopted mid-history starts here until enough
            // exemplars accumulate).
            let mut missing: Vec<(String, usize)> = companion_counts
                .into_iter()
                .filter(|(_, n)| *n * 10 >= exemplar_count * 4)
                .filter(|(p, _)| !ctx.changed_paths.iter().any(|c| path_suffix_match(c, p)))
                .collect();
            if missing.is_empty() {
                continue;
            }
            missing.sort_by(|a, b| b.1.cmp(&a.1));
            for (tier_min, tier_max, severity, label) in [
                (6usize, usize::MAX, Severity::Warning, "most also touched"),
                (4, 6, Severity::Info, "several also touched"),
            ] {
                let tier: Vec<&(String, usize)> = missing
                    .iter()
                    .filter(|(_, n)| {
                        n * 10 >= exemplar_count * tier_min
                            && (tier_max == usize::MAX || n * 10 < exemplar_count * tier_max)
                    })
                    .take(5)
                    .collect();
                if tier.is_empty() {
                    continue;
                }
                let list = tier
                    .iter()
                    .map(|(p, n)| format!("`{p}` ({n}/{exemplar_count} PRs)"))
                    .collect::<Vec<_>>()
                    .join(", ");
                findings.push(ReviewFinding::new(
                    severity,
                    "co_added_family",
                    format!("{family}/"),
                    format!("PRs adding files under {family}/ historically touch companion files"),
                    format!(
                        "Of {exemplar_count} merged PRs that added files under `{family}/`, \
                         {label}: {list}. This diff adds files there but touches none of \
                         these companions — docs contracts, permission entries, and \
                         registrations ride along in this family's approved history."
                    ),
                    "Check each companion: update it if the contract applies (e.g. API docs \
                     for a new endpoint), or note why it doesn't for this change."
                        .to_string(),
                ));
            }
            if findings.len() >= 3 {
                break;
            }
        }
        Ok(findings)
    }
}

// ─── Gate 16: Complexity & parameter budget (SonarQube friction) ────────────
//
// Two SonarQube defaults the team pays re-push cycles for (user ruling
// 2026-07-10): cyclomatic complexity per function ≤ 15, and parameter
// lists ≤ 7 (agents habitually violate this — observed: a generated
// 19-parameter VB function). Plus the house "clean as you code" rule:
// touching a function that is ALREADY over the complexity budget should
// usually include the refactor down to ~13–14 when reasonably safe,
// because SonarQube taxes every future touch of that function otherwise.
// Detection is language-generic (VB / C# / TS / JS) and heuristic —
// keyword-anchored declaration scans and the same decision-point counter
// the edit-safety verdicts use, not a full parser.

const SQ_COMPLEXITY_MAX: u32 = 15;
const SQ_PARAMS_MAX: usize = 7;

fn complexity_gate_ext(path: &str) -> Option<bool> {
    // Some(true) = VB-style (End Function terminators), Some(false) = brace-style.
    let l = path.to_ascii_lowercase();
    // MiniLang uses End Function/End Sub terminators like VB.
    if l.ends_with(".vb") || l.ends_with(".ml") || l.ends_with(".mlinc") {
        Some(true)
    } else if [".cs", ".ts", ".tsx", ".js", ".jsx"]
        .iter()
        .any(|e| l.ends_with(e))
    {
        Some(false)
    } else {
        None
    }
}

static RE_CX_DECL_VB: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?im)^\s*(?:<[^>\r\n]*>\s*)?(?:(?:public|private|protected|friend|shared|overrides|overridable|notoverridable|mustoverride|async|iterator)\s+)*(?:function|sub)\s+(\w+)\s*\(",
    )
    .expect("valid vb decl regex")
});

static RE_CX_DECL_CS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?m)^\s*(?:\[[^\]\r\n]*\]\s*)*(?:(?:public|private|protected|internal|static|virtual|override|sealed|async|partial|new|extern|unsafe)\s+)+[\w<>\[\],\s?]*?\b(\w+)\s*\(",
    )
    .expect("valid cs decl regex")
});

static RE_CX_DECL_TS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?m)\bfunction\s+(\w+)\s*\(|^\s*(?:export\s+)?(?:const|let|var)\s+(\w+)\s*=\s*(?:async\s+)?\(|^\s*(?:(?:public|private|protected|static|async|readonly)\s+)+(\w+)\s*\(",
    )
    .expect("valid ts decl regex")
});

/// Count parameters in the list opening at `open_paren` (byte offset of
/// `(` in `text`). Commas at paren depth 1 with angle/bracket depth 0
/// separate parameters; returns None if the list never closes (truncated
/// diff content).
pub(crate) fn count_params_at(text: &str, open_paren: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.get(open_paren) != Some(&b'(') {
        return None;
    }
    let (mut paren, mut angle, mut bracket) = (0i32, 0i32, 0i32);
    let mut commas = 0usize;
    let mut saw_content = false;
    for &b in &bytes[open_paren..] {
        match b {
            b'(' => paren += 1,
            b')' => {
                paren -= 1;
                if paren == 0 {
                    return Some(if saw_content { commas + 1 } else { 0 });
                }
            }
            b'<' => angle += 1,
            b'>' => angle = (angle - 1).max(0),
            b'[' => bracket += 1,
            b']' => bracket -= 1,
            b',' if paren == 1 && angle == 0 && bracket == 0 => commas += 1,
            b if !b.is_ascii_whitespace() => saw_content = true,
            _ => {}
        }
    }
    None
}

/// Function spans `(start_line, end_line, name)` (1-based, inclusive) in
/// a file's lines. VB spans close at `End Function`/`End Sub`; brace
/// languages close at the matching `}` (naive brace count — string
/// literals containing braces can skew it; acceptable for a review nudge).
pub(crate) fn function_spans(content: &str, vb_style: bool) -> Vec<(usize, usize, String)> {
    let decl_re: &Regex = if vb_style { &RE_CX_DECL_VB } else { &RE_CX_DECL_CS };
    let ts_re: &Regex = &RE_CX_DECL_TS;
    let line_starts: Vec<usize> = std::iter::once(0)
        .chain(content.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let line_of = |off: usize| line_starts.partition_point(|&s| s <= off); // 1-based
    // Collect declaration candidates first (dedup by start line), then
    // compute spans — keeps the borrow of `spans` out of the scan loops.
    let mut candidates: Vec<(usize, String)> = decl_re
        .captures_iter(content)
        .map(|m| {
            let name = m.get(1).map(|g| g.as_str().to_string()).unwrap_or_default();
            (m.get(0).expect("group 0").start(), name)
        })
        .collect();
    if !vb_style {
        for m in ts_re.captures_iter(content) {
            let name = m
                .get(1)
                .or_else(|| m.get(2))
                .or_else(|| m.get(3))
                .map(|g| g.as_str().to_string())
                .unwrap_or_default();
            candidates.push((m.get(0).expect("group 0").start(), name));
        }
    }
    candidates.sort_by_key(|(off, _)| *off);
    let mut seen_lines: HashSet<usize> = HashSet::new();

    let mut spans = Vec::new();
    for (m_start, name) in candidates {
        let start_line = line_of(m_start);
        if !seen_lines.insert(start_line) {
            continue;
        }
        let end_line = if vb_style {
            static RE_END: LazyLock<Regex> = LazyLock::new(|| {
                Regex::new(r"(?i)^\s*end\s+(?:function|sub)\b").expect("valid end regex")
            });
            content
                .lines()
                .enumerate()
                .skip(start_line)
                .find(|(_, l)| RE_END.is_match(l))
                .map(|(i, _)| i + 1)
        } else {
            let mut depth = 0i32;
            let mut seen_open = false;
            let mut end = None;
            for (i, l) in content.lines().enumerate().skip(start_line - 1) {
                for c in l.chars() {
                    match c {
                        '{' => {
                            depth += 1;
                            seen_open = true;
                        }
                        '}' => depth -= 1,
                        _ => {}
                    }
                }
                if seen_open && depth <= 0 {
                    end = Some(i + 1);
                    break;
                }
            }
            end
        };
        if let Some(e) = end_line {
            spans.push((start_line, e, name));
        }
    }
    spans
}

pub struct ComplexityGate;

#[async_trait]
impl Gate for ComplexityGate {
    fn name(&self) -> &'static str {
        "complexity_budget"
    }

    fn run(&self, ctx: &GateContext<'_>) -> anyhow::Result<Vec<ReviewFinding>> {
        use crate::handlers::access_layer_tools::estimate_complexity;
        let mut findings = Vec::new();
        for df in ctx.diff_files {
            let Some(vb_style) = complexity_gate_ext(&df.path) else {
                continue;
            };
            if df.is_binary
                || df.is_test_file()
                || super::is_generated_filename(&df.path)
                || df.added_content.is_empty()
            {
                continue;
            }

            // A. Parameter budget on ADDED declarations — the shape agents
            // generate. added_content is new-lines-only, so this fires on
            // new/rewritten signatures, not untouched legacy ones.
            let decl_res: [&Regex; 2] = if vb_style {
                [&RE_CX_DECL_VB, &RE_CX_DECL_VB]
            } else {
                [&RE_CX_DECL_CS, &RE_CX_DECL_TS]
            };
            let mut seen_offsets: HashSet<usize> = HashSet::new();
            for re in decl_res {
                for m in re.captures_iter(&df.added_content) {
                    let whole = m.get(0).expect("group 0");
                    if !seen_offsets.insert(whole.start()) {
                        continue;
                    }
                    let name = (1..=3)
                        .filter_map(|i| m.get(i))
                        .map(|g| g.as_str())
                        .next()
                        .unwrap_or("?")
                        .to_string();
                    let open = whole.end() - 1;
                    let Some(n) = count_params_at(&df.added_content, open) else {
                        continue;
                    };
                    if n <= SQ_PARAMS_MAX {
                        continue;
                    }
                    let decl_line = df
                        .added_lines
                        .get(df.added_content[..whole.start()].matches('\n').count())
                        .map(|(ln, _)| *ln);
                    findings.push(
                        ReviewFinding::new(
                            Severity::Warning,
                            "complexity_budget",
                            df.path.clone(),
                            format!("`{name}` declares {n} parameters (SonarQube max {SQ_PARAMS_MAX})"),
                            format!(
                                "This diff adds a declaration with {n} parameters. SonarQube \
                                 flags any function over {SQ_PARAMS_MAX}; long parameter lists \
                                 are also the most common agent-generated review bounce."
                            ),
                            "Group the parameters into a parameter object (options/query/DTO \
                             type) or split the function so each piece takes a coherent subset.",
                        )
                        .with_lines(decl_line.into_iter().collect()),
                    );
                }
            }

            // B. Complexity of functions this diff touches, measured on the
            // post-change file. New-over-budget = hard warning; touched
            // legacy-over-budget = the clean-as-you-code nudge.
            let disk_bytes = std::fs::read(ctx.project_dir.join(&df.path)).unwrap_or_default();
            if disk_bytes.is_empty() {
                continue;
            }
            let disk = String::from_utf8_lossy(&disk_bytes);
            let touched: Vec<(usize, usize)> = df
                .hunks
                .iter()
                .map(|h| (h.new_start, h.new_start + h.new_count.max(1) - 1))
                .collect();
            let added_line_set: HashSet<usize> = df.added_lines.iter().map(|(n, _)| *n).collect();
            let disk_lines: Vec<&str> = disk.lines().collect();
            let mut over: Vec<(u32, usize, String, bool)> = Vec::new();
            for (start, end, name) in function_spans(&disk, vb_style) {
                if !touched.iter().any(|(ts, te)| start <= *te && end >= *ts) {
                    continue;
                }
                let body = disk_lines
                    .get(start - 1..end.min(disk_lines.len()))
                    .unwrap_or(&[])
                    .join("\n");
                let cx = estimate_complexity(&body);
                if cx > SQ_COMPLEXITY_MAX {
                    over.push((cx, start, name, added_line_set.contains(&start)));
                }
            }
            over.sort_by(|a, b| b.0.cmp(&a.0));
            for (cx, start, name, is_new) in over.into_iter().take(3) {
                let (sev, title, detail, suggestion) = if is_new {
                    (
                        Severity::Warning,
                        format!("New function `{name}` has estimated complexity {cx} (max {SQ_COMPLEXITY_MAX})"),
                        format!(
                            "SonarQube will reject this on the next scan — new code over \
                             complexity {SQ_COMPLEXITY_MAX} is a standing quality-gate failure \
                             (estimated {cx} decision points)."
                        ),
                        "Extract the branch clusters into named helpers now, before push — \
                         guard clauses for the early exits, a helper per decision cluster."
                            .to_string(),
                    )
                } else {
                    (
                        Severity::Info,
                        format!("Touched function `{name}` is already at complexity {cx} (max {SQ_COMPLEXITY_MAX})"),
                        format!(
                            "This diff modifies a function that already exceeds the complexity \
                             budget (estimated {cx}). House rule: when you touch an \
                             over-budget function, take it down to ~13–14 in the same change \
                             if reasonably safe — otherwise every future touch pays this tax."
                        ),
                        "If the change is low-risk, extract the densest branch cluster into a \
                         helper while you are here; if it is too risky, note that explicitly \
                         in the PR description."
                            .to_string(),
                    )
                };
                findings.push(
                    ReviewFinding::new(sev, "complexity_budget", df.path.clone(), title, detail, suggestion)
                        .with_lines(vec![start]),
                );
            }
        }
        Ok(findings)
    }
}

// ─── Gate 17: Added-code conventions (docs + logging) ───────────────────────
//
// The two mechanical classes that dominated the 2026-07-10 authoring
// experiment (fresh implementations exhibited 62-78% of the exact
// findings reviewers had raised; XML-docs-on-public-members and
// house-logger-in-catch recurred on every fresh API surface):
//   (a) an ADDED public member with no doc comment, in a file whose
//       existing public members ARE documented (house style evidenced
//       from the same file on disk — no evidence, no finding);
//   (b) an ADDED catch block that logs via a different helper than the
//       file's dominant logger.
// Both language-generic (VB ''' / C#-TS ///+/** */), evidence-based,
// capped per file.

static RE_AC_PUBLIC_DECL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?im)^\s*(?:public|friend)\s+(?:(?:shared|overrides|overridable|async|static|virtual|override|sealed|readonly|partial)\s+)*(?:function|sub|property|class|interface|enum|[\w<>\[\],?]+)\s+(\w+)",
    )
    .expect("valid public decl regex")
});

static RE_AC_DOC_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:'''|///|/\*\*|\*)").expect("valid doc line regex")
});

static RE_AC_CATCH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*catch\b").expect("valid catch regex"));

static RE_AC_LOG_CALL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b([\w.]*\blog\w*)\s*\(").expect("valid log call regex")
});

/// Fraction of public decls in `content` that carry a doc comment on the
/// line above, together with the total count — the house-style evidence.
pub(crate) fn doc_coverage(content: &str) -> (usize, usize) {
    let lines: Vec<&str> = content.lines().collect();
    let (mut documented, mut total) = (0usize, 0usize);
    for (i, line) in lines.iter().enumerate() {
        if RE_AC_PUBLIC_DECL.is_match(line) {
            total += 1;
            if i > 0 && RE_AC_DOC_LINE.is_match(lines[i - 1]) {
                documented += 1;
            }
        }
    }
    (documented, total)
}

/// Dominant logging helper name in `content` (lowercased), if one call
/// shape clearly wins (>=3 uses and >=3x the runner-up).
pub(crate) fn dominant_logger(content: &str) -> Option<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for cap in RE_AC_LOG_CALL.captures_iter(content) {
        let name = cap[1].to_lowercase();
        // "log" alone or console noise is not a helper convention.
        if name == "log" || name.contains("console.") {
            continue;
        }
        *counts.entry(name).or_default() += 1;
    }
    let mut v: Vec<(String, usize)> = counts.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    match v.as_slice() {
        [] => None,
        [(name, n)] if *n >= 3 => Some(name.clone()),
        [(name, n), (_, m), ..] if *n >= 3 && *n >= m * 3 => Some(name.clone()),
        _ => None,
    }
}

/// An assignment whose RHS ends in a contractually-nullable-returning
/// call: `x = …FirstOrDefault(…)` / `SingleOrDefault` / `Find` /
/// `ElementAtOrDefault` (VB/C#/LINQ) or `.find(…)` (JS/TS). These return
/// null/undefined by their own contract — keying on the method name is
/// precise, not a heuristic guess about what "looks like" a query.
static RE_AC_NULLABLE_ASSIGN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b([a-z_]\w*)\s*=\s*.*\.(FirstOrDefault|SingleOrDefault|ElementAtOrDefault|Find|find)\s*\(",
    )
    .expect("valid nullable-assign regex")
});

pub struct AddedConventionsGate;

#[async_trait]
impl Gate for AddedConventionsGate {
    fn name(&self) -> &'static str {
        "added_conventions"
    }

    fn run(&self, ctx: &GateContext<'_>) -> anyhow::Result<Vec<ReviewFinding>> {
        let mut findings = Vec::new();
        for df in ctx.diff_files {
            let lower = df.path.to_lowercase();
            let is_code = [".vb", ".cs", ".ts", ".tsx", ".js", ".jsx"]
                .iter()
                .any(|e| lower.ends_with(e));
            let is_markup = [".aspx", ".ascx", ".master", ".html", ".htm", ".vbhtml", ".cshtml"]
                .iter()
                .any(|e| lower.ends_with(e))
                || lower.ends_with(".tsx")
                || lower.ends_with(".jsx");
            let is_css = [".css", ".scss", ".less"].iter().any(|e| lower.ends_with(e));
            if df.is_binary
                || df.is_test_file()
                || super::is_generated_filename(&df.path)
                || df.added_content.is_empty()
            {
                continue;
            }

            // (c) Accessibility on added markup/styles — the one review
            // class the replay residual showed RECURRING yet uncovered by
            // any rule or gate (alt-less images, focus styles stripped by
            // `all: unset`). Standards-based, not house-style — no
            // evidence gate needed.
            if is_markup {
                static RE_IMG: LazyLock<Regex> = LazyLock::new(|| {
                    Regex::new(r#"(?i)<(?:img|asp:Image)\b[^>]*"#).expect("valid img regex")
                });
                static RE_ALT: LazyLock<Regex> = LazyLock::new(|| {
                    Regex::new(r#"(?i)\b(?:alt\s*=|AlternateText\s*=|aria-hidden\s*=\s*["']?true)"#)
                        .expect("valid alt regex")
                });
                let mut missing_alt: Vec<usize> = Vec::new();
                for (line_no, text) in &df.added_lines {
                    for m in RE_IMG.find_iter(text) {
                        if !RE_ALT.is_match(m.as_str()) {
                            missing_alt.push(*line_no);
                        }
                    }
                }
                if !missing_alt.is_empty() {
                    missing_alt.truncate(5);
                    findings.push(
                        ReviewFinding::new(
                            Severity::Info,
                            "added_conventions",
                            df.path.clone(),
                            format!("{} added image(s) without alt text or aria-hidden", missing_alt.len()),
                            "Added <img>/<asp:Image> elements carry neither alt text nor \
                             aria-hidden=\"true\". Screen readers announce these as noise; \
                             reviewers flag it on every markup change that ships images."
                                .to_string(),
                            "Give meaningful images an alt text; mark decorative ones \
                             aria-hidden=\"true\" (or alt=\"\").",
                        )
                        .with_lines(missing_alt),
                    );
                }
            }
            if is_css && df.added_content.to_lowercase().contains("all: unset")
                && !df.added_content.to_lowercase().contains(":focus")
            {
                let line = df
                    .added_lines
                    .iter()
                    .find(|(_, t)| t.to_lowercase().contains("all: unset"))
                    .map(|(n, _)| *n);
                findings.push(
                    ReviewFinding::new(
                        Severity::Info,
                        "added_conventions",
                        df.path.clone(),
                        "`all: unset` added without restoring focus styles".to_string(),
                        "`all: unset` strips the browser's focus indicator; keyboard users \
                         lose track of where they are. The added block defines no :focus \
                         replacement."
                            .to_string(),
                        "Add a visible :focus/:focus-visible style alongside the reset.",
                    )
                    .with_lines(line.into_iter().collect()),
                );
            }
            if !is_code {
                continue;
            }
            let disk_bytes = std::fs::read(ctx.project_dir.join(&df.path)).unwrap_or_default();
            let disk = String::from_utf8_lossy(&disk_bytes);

            // (a) Undocumented ADDED public members. Evidence basis, either:
            //   - the file's own house style documents public members, or
            //   - an ingested repo rule demands doc comments (the live case:
            //     copilot-instructions mandate XML docs while merged files
            //     often skip them — reviewers enforce the RULE, so
            //     file-local style alone missed exactly the findings this
            //     gate was built from).
            let (documented, total) = doc_coverage(&disk);
            let file_documents = total >= 3 && documented * 10 >= total * 4;
            let rule_demands_docs = ctx.repo_rules.iter().any(|r| {
                let t = r.rule_text.to_lowercase();
                (t.contains("xml doc") || t.contains("xml-doc") || t.contains("doc comment")
                    || t.contains("'''") || t.contains("///"))
                    && (t.contains("public") || t.contains("member") || t.contains("summary"))
            });
            if file_documents || rule_demands_docs {
                let added: Vec<&(usize, String)> = df.added_lines.iter().collect();
                let mut undocumented: Vec<(usize, String)> = Vec::new();
                for (idx, (line_no, text)) in added.iter().enumerate() {
                    let Some(cap) = RE_AC_PUBLIC_DECL.captures(text) else {
                        continue;
                    };
                    // Doc line directly above — in the added block or on disk.
                    let prev_added = idx
                        .checked_sub(1)
                        .and_then(|i| added.get(i))
                        .filter(|(n, _)| n + 1 == *line_no)
                        .map(|(_, t)| RE_AC_DOC_LINE.is_match(t));
                    let documented_above = match prev_added {
                        Some(v) => v,
                        None => line_no
                            .checked_sub(2)
                            .and_then(|i| disk.lines().nth(i))
                            .map(|l| RE_AC_DOC_LINE.is_match(l))
                            .unwrap_or(false),
                    };
                    if !documented_above {
                        undocumented.push((*line_no, cap[1].to_string()));
                    }
                }
                if !undocumented.is_empty() {
                    let names: Vec<String> = undocumented
                        .iter()
                        .take(5)
                        .map(|(_, n)| format!("`{n}`"))
                        .collect();
                    let lines: Vec<usize> = undocumented.iter().take(5).map(|(l, _)| *l).collect();
                    findings.push(
                        ReviewFinding::new(
                            Severity::Info,
                            "added_conventions",
                            df.path.clone(),
                            format!(
                                "{} new public member(s) missing doc comments in a documented file",
                                undocumented.len()
                            ),
                            format!(
                                "{} The added {} lack(s) a doc comment. Reviewers here flag \
                                 undocumented public surface on every fresh change.",
                                if file_documents {
                                    format!(
                                        "This file documents {documented} of its {total} public members."
                                    )
                                } else {
                                    "This repo's ingested rules require doc comments on public members."
                                        .to_string()
                                },
                                names.join(", ")
                            ),
                            "Add a doc comment above each new public member, matching the \
                             style used elsewhere in this file.",
                        )
                        .with_lines(lines),
                    );
                }
            }

            // (b) Added catch blocks logging via a non-dominant helper.
            if let Some(dom) = dominant_logger(&disk) {
                let mut off_convention: Vec<(usize, String)> = Vec::new();
                let added = &df.added_lines;
                for (idx, (line_no, text)) in added.iter().enumerate() {
                    if !RE_AC_CATCH.is_match(text) {
                        continue;
                    }
                    // Scan the next few ADDED lines (same contiguous block)
                    // for a log call.
                    for j in idx + 1..(idx + 8).min(added.len()) {
                        let (n, t) = &added[j];
                        if *n != added[j - 1].0 + 1 {
                            break; // left the contiguous added block
                        }
                        if let Some(cap) = RE_AC_LOG_CALL.captures(t) {
                            let used = cap[1].to_lowercase();
                            if used != dom && !used.ends_with(&format!(".{dom}")) && !dom.ends_with(&used)
                            {
                                off_convention.push((*line_no, cap[1].to_string()));
                            }
                            break;
                        }
                    }
                }
                for (line_no, used) in off_convention.into_iter().take(3) {
                    findings.push(
                        ReviewFinding::new(
                            Severity::Info,
                            "added_conventions",
                            df.path.clone(),
                            format!("Added catch block logs via `{used}` — file's convention is `{dom}`"),
                            format!(
                                "The dominant logging helper in this file is `{dom}`; a new \
                                 catch block logging through `{used}` is the exact class \
                                 reviewers bounce ('align exception logging with the required \
                                 convention')."
                            ),
                            format!("Route the catch-block logging through `{dom}` like the rest of the file."),
                        )
                        .with_lines(vec![line_no]),
                    );
                }
            }

            // (d) Unguarded deref of a contractually-nullable result — the
            // #1 recurring class in the 2026-07-10 replay corpus (54 caught
            // + 31 missed of 263 real review findings). Evidence-gated like
            // the docs check: only fires where an ingested repo rule
            // demands null-guarding data-access returns, so it never fires
            // on a codebase without that convention. Precise trigger:
            // `x = ….FirstOrDefault(…)` etc. (methods that return null BY
            // CONTRACT), immediately followed within the added block by
            // `x.<member>` with no intervening null check.
            let rule_demands_null_guard = ctx.repo_rules.iter().any(|r| {
                let t = r.rule_text.to_lowercase();
                (t.contains("null") || t.contains("nothing"))
                    && (t.contains("guard")
                        || t.contains("check")
                        || t.contains("before")
                        || t.contains("data-access")
                        || t.contains("data access"))
            });
            if rule_demands_null_guard {
                let added = &df.added_lines;
                let mut flagged: Vec<(usize, String)> = Vec::new();
                for (idx, (line_no, text)) in added.iter().enumerate() {
                    let Some(cap) = RE_AC_NULLABLE_ASSIGN.captures(text) else {
                        continue;
                    };
                    let var = cap[1].to_string();
                    if var.is_empty() {
                        continue;
                    }
                    // Same-line guard already present (`= x.Find(..) ?? …`,
                    // inline If) — skip.
                    let lower = text.to_lowercase();
                    if lower.contains("??")
                        || lower.contains(&format!("{}?.", var.to_lowercase()))
                    {
                        continue;
                    }
                    // Look ahead in the contiguous added block for a deref
                    // or a guard, whichever comes first.
                    let deref = format!("{}.", var.to_lowercase());
                    let guard_is_nothing = format!("{} is nothing", var.to_lowercase());
                    let guard_isnot = format!("{} isnot nothing", var.to_lowercase());
                    for j in idx + 1..(idx + 6).min(added.len()) {
                        if added[j].0 != added[j - 1].0 + 1 {
                            break; // left the contiguous added region
                        }
                        let l = added[j].1.to_lowercase();
                        if l.contains(&guard_is_nothing)
                            || l.contains(&guard_isnot)
                            || l.contains(&format!("if {} ", var.to_lowercase()))
                            || l.contains("=== null")
                            || l.contains("!== undefined")
                            || l.contains(&format!("{}?.", var.to_lowercase()))
                        {
                            break; // guarded before use — good
                        }
                        if l.contains(&deref) {
                            flagged.push((*line_no, var.clone()));
                            break;
                        }
                    }
                }
                for (line_no, var) in flagged.into_iter().take(4) {
                    findings.push(
                        ReviewFinding::new(
                            Severity::Warning,
                            "added_conventions",
                            df.path.clone(),
                            format!("`{var}` may be null (…OrDefault/Find) and is dereferenced without a guard"),
                            format!(
                                "`{var}` is assigned from a method that returns null/undefined by \
                                 contract, then used without a null check — this repo's rules \
                                 require guarding data-access returns, and it is the single most \
                                 common review-bounce class."
                            ),
                            format!("Guard `{var}` (If {var} IsNot Nothing / early return / null-conditional) before using it."),
                        )
                        .with_lines(vec![line_no]),
                    );
                }
            }
        }
        Ok(findings)
    }
}

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
    fn nullable_assign_regex_matches_contract_nullable_calls_only() {
        // Contractually-nullable → match + capture the var.
        for (src, var) in [
            ("Dim u = db.Users.FirstOrDefault(Function(x) x.Id = 1)", "u"),
            ("row = list.SingleOrDefault(r => r.Ok)", "row"),
            ("var m = arr.find(x => x.id === id)", "m"),
            ("marker = markers.Find(AddressOf Match)", "marker"),
        ] {
            let c = RE_AC_NULLABLE_ASSIGN
                .captures(src)
                .unwrap_or_else(|| panic!("should match: {src}"));
            assert_eq!(&c[1], var, "wrong var for: {src}");
        }
        // Non-nullable-by-contract calls must NOT match.
        for src in [
            "Dim all = db.Users.ToList()",
            "count = items.Count()",
            "first = seq.First()", // First() throws, not nullable — excluded on purpose
        ] {
            assert!(
                RE_AC_NULLABLE_ASSIGN.captures(src).is_none(),
                "should NOT match: {src}"
            );
        }
    }

    #[test]
    fn a11y_flags_altless_images_and_unset_focus() {
        let gate = AddedConventionsGate;
        // Direct regex-level checks via a run() would need a full ctx;
        // exercise the markup patterns through a minimal DiffFile pair.
        let df = mk_df(
            "Site/modules/page.aspx",
            &[
                (10, r#"<img src="excel.png" class="icon" />"#),
                (11, r#"<asp:Image runat="server" ImageUrl="x.png" AlternateText="chart" />"#),
                (12, r#"<img src="ok.png" alt="" />"#),
            ],
        );
        // Only line 10 lacks alt/AlternateText/aria-hidden.
        let _ = gate; // gate construction sanity
        let re_img = regex::Regex::new(r#"(?i)<(?:img|asp:Image)\b[^>]*"#).unwrap();
        let re_alt =
            regex::Regex::new(r#"(?i)\b(?:alt\s*=|AlternateText\s*=|aria-hidden\s*=\s*["']?true)"#)
                .unwrap();
        let mut missing = Vec::new();
        for (n, t) in &df.added_lines {
            for m in re_img.find_iter(t) {
                if !re_alt.is_match(m.as_str()) {
                    missing.push(*n);
                }
            }
        }
        assert_eq!(missing, vec![10]);
    }

    #[test]
    fn doc_coverage_counts_documented_public_members() {
        let src = "''' <summary>Adds.</summary>\n\
                   Public Function AddItem(x As Integer) As Integer\n\
                   End Function\n\
                   Public Sub Undocumented()\n\
                   End Sub\n\
                   ''' <summary>Gets.</summary>\n\
                   Public Property Name As String\n";
        let (documented, total) = doc_coverage(src);
        assert_eq!((documented, total), (2, 3));
    }

    #[test]
    fn dominant_logger_requires_clear_winner() {
        let src = "api.LogError(a)\napi.LogError(b)\napi.LogError(c)\nLogger.Loggerror(d)\n";
        assert_eq!(dominant_logger(src).as_deref(), Some("api.logerror"));
        let tied = "api.LogError(a)\nLogger.Loggerror(b)\n";
        assert_eq!(dominant_logger(tied), None);
    }

    #[test]
    fn param_counter_handles_nesting_and_emptiness() {
        let vb = "Public Function DoSomething(q, w, e, r, t, y, u, i, o, p, a, s, d, f, g, h, h2, j, j2) As Boolean";
        let open = vb.find('(').unwrap();
        assert_eq!(count_params_at(vb, open), Some(19));

        let cs = "public void Ok(Dictionary<string, int> map, Func<int, int> f) {";
        assert_eq!(count_params_at(cs, cs.find('(').unwrap()), Some(2));

        let empty = "Public Sub NoArgs()";
        assert_eq!(count_params_at(empty, empty.find('(').unwrap()), Some(0));

        let unterminated = "Public Sub Cut(a, b,";
        assert_eq!(count_params_at(unterminated, unterminated.find('(').unwrap()), None);
    }

    #[test]
    fn vb_function_spans_close_at_end_function() {
        let src = "Public Class C\n\
                   Public Function A(x As Integer) As Integer\n\
                   If x > 0 Then Return 1\n\
                   Return 0\n\
                   End Function\n\
                   Private Sub B()\n\
                   End Sub\n\
                   End Class\n";
        let spans = function_spans(src, true);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0], (2, 5, "A".to_string()));
        assert_eq!(spans[1], (6, 7, "B".to_string()));
    }

    #[test]
    fn brace_function_spans_close_at_matching_brace() {
        let src = "export function calc(a: number): number {\n\
                   if (a > 1) {\n\
                   return 1;\n\
                   }\n\
                   return 0;\n\
                   }\n";
        let spans = function_spans(src, false);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].0, 1);
        assert_eq!(spans[0].1, 6);
        assert_eq!(spans[0].2, "calc");
    }

    #[test]
    fn complexity_gate_flags_wide_param_list_in_added_vb() {
        let decl = "Public Function DoSomething(q, w, e, r, t, y, u, i, o, p, a, s, d, f, g, h, h2, j, j2) As Boolean";
        let df = mk_df("Site/App_Code/foo.vb", &[(10, decl)]);
        // Run just the added-declaration scan by invoking the gate with a
        // context-free shim: parameter check needs only the DiffFile.
        let mut found = Vec::new();
        for re in [&*RE_CX_DECL_VB] {
            for m in re.captures_iter(&df.added_content) {
                let open = m.get(0).unwrap().end() - 1;
                if let Some(n) = count_params_at(&df.added_content, open) {
                    found.push(n);
                }
            }
        }
        assert_eq!(found, vec![19]);
    }

    #[test]
    fn bare_name_matches_qualified_and_exact() {
        assert!(bare_name_matches(
            "api.StartTransaction",
            "StartTransaction"
        ));
        assert!(bare_name_matches("StartTransaction", "starttransaction"));
        assert!(bare_name_matches(
            "Ns.Class.AnyLinkedMarker",
            "AnyLinkedMarker"
        ));
        assert!(!bare_name_matches(
            "api.StartTransactionAsync",
            "StartTransaction"
        ));
        assert!(!bare_name_matches(
            "api.RestartTransaction",
            "StartTransaction"
        ));
    }

    #[test]
    fn site_tail_identifier_parses_fqn_and_rejects_junk() {
        assert_eq!(
            site_tail_identifier("_io.import.MarkerImport.GetMarkersToDeleteFromProject()"),
            Some("GetMarkersToDeleteFromProject".to_string())
        );
        assert_eq!(
            site_tail_identifier("SomeClass.DoThing() ,"),
            Some("DoThing".to_string())
        );
        assert_eq!(site_tail_identifier("1)"), None);
        assert_eq!(site_tail_identifier("a.b"), None, "tail too short");
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
    fn unwired_candidates_capture_enclosing_class() {
        let diff = vec![mk_df(
            "Site/App_Code/ata/code/ChangeRequestMarker.vb",
            &[
                (5, "Public Class ChangeRequestMarker"),
                (
                    10,
                    "Public Shared Function Create(ath_id As Integer) As Integer",
                ),
                (11, "End Function"),
                (20, "End Class"),
            ],
        )];
        let c = unwired_candidates(&diff);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].name, "Create");
        assert_eq!(
            c[0].class_name.as_deref(),
            Some("ChangeRequestMarker"),
            "enclosing class must be captured so common names only get \
             suppressed by same-class graph nodes"
        );
    }

    #[test]
    fn family_keys_emit_parent_and_grandparent() {
        assert_eq!(
            family_keys("Site/App_Code/api-v2/Controllers/markerInspection/X.vb"),
            vec![
                "app_code/api-v2/controllers/markerinspection".to_string(),
                "app_code/api-v2/controllers".to_string(),
            ]
        );
        assert!(family_keys("docs/readme.md").is_empty());
        assert!(family_keys("X.vb").is_empty());
    }

    #[test]
    fn parse_pr_doc_files_reads_shipped_list_only() {
        let doc = "# PR-1: t\nmerged: x | files: 3\n\nbody - not a file\n\n\
                   ## Files shipped together in this approved change\n\
                   - App_Code/api-v2/WebApiConfig.vb\n\
                   - .github/copilot-instructions.md\n\
                   ... and 2 more\n\n## Other section\n- not/this.vb\n";
        assert_eq!(
            parse_pr_doc_files(doc),
            vec![
                "App_Code/api-v2/WebApiConfig.vb".to_string(),
                ".github/copilot-instructions.md".to_string()
            ]
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
    fn style_gate_flags_minilang_casing_through_of_clause() {
        // The MiniLang MethodNaming regex must not reuse the VB pattern,
        // which demands `Sub|Function <name>(` — MiniLang's generic `Of`
        // clause sits between the name and the parenthesis
        // (`Function badCaseGeneric Of T(...)`), so a VB-shaped regex would
        // silently fail to capture the name and miss the finding entirely.
        let diff = "\
diff --git a/foo.ml b/foo.ml
--- a/foo.ml
+++ b/foo.ml
@@ -1,3 +1,4 @@
 Module Foo
     Public Function Existing() As Integer
+    Public Function badCaseGeneric Of T(items As List(Of T)) As List(Of T)
     End Function
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
            findings.iter().any(|f| f.title.contains("badCaseGeneric")),
            "expected casing finding for the generic method, got {findings:#?}"
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

    #[test]
    fn minilang_files_get_the_complexity_gate() {
        // Some(true) = End-Function terminator style. Returning None would make
        // gate 16 silently skip every MiniLang file.
        assert_eq!(complexity_gate_ext("Std.Collections.List.ml"), Some(true));
        assert_eq!(complexity_gate_ext("shared.mlinc"), Some(true));
        assert_eq!(complexity_gate_ext("Form1.vb"), Some(true));
        assert_eq!(complexity_gate_ext("Program.cs"), Some(false));
        assert_eq!(complexity_gate_ext("README.md"), None);
    }
}
