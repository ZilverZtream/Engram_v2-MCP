//! Project-memory generator — renders `CLAUDE.md` + `.claude/rules/*.md`
//! from data already in the graph.
//!
//! Design: this module contains ONLY pure data shaping + string rendering.
//! The handler in `cognitive_tools.rs` gathers facts from the live graph
//! (language counts, blast radius, repo rules, session workflows, …) and
//! hands the caller a fully-populated [`ProjectSnapshot`]. The renderers
//! here are deterministic and unit-testable without a `GraphStore`.
//!
//! Sections with no data are omitted entirely — a Rust CLI project gets
//! language conventions + danger zones + co-change pairs, a WebForms
//! project gets every section.

pub mod llm_curation;
pub mod rules_pipeline;

use std::fmt::Write;

// ── Data model ───────────────────────────────────────────────────────────────

/// One entry in the language breakdown. `share_percent` is the file count
/// as a percentage of the total — the renderer uses this to decide which
/// languages get their own rules file (default threshold: 5%).
#[derive(Debug, Clone)]
pub struct LanguageShare {
    pub language: String,
    pub file_count: usize,
    pub share_percent: f32,
}

#[derive(Debug, Clone)]
pub struct DangerZone {
    pub file_path: String,
    pub risk_score: u8,
    pub risk_band: String,
    pub total_downstream: usize,
    /// Short phrase summarising WHY this file is dangerous — derived
    /// deterministically from the blast-radius breakdown. Examples:
    /// "state+events", "immune-flagged", "auto-generated", "schema hub".
    pub reasons: Vec<String>,
}

/// A hard-learned rule sourced from a repo rule (immune flag, anti-pattern
/// from revert) or copied from an existing human-authored CLAUDE.md.
#[derive(Debug, Clone)]
pub struct CriticalRule {
    /// Actionable one-liner. Ends in a full stop. Never generic.
    pub text: String,
    /// Machine-readable evidence suffix — "(revert b4f76e01)" or
    /// "(168 callers, blast radius 4/10)". Optional.
    pub evidence: Option<String>,
    /// Source provenance so we can dedupe against existing CLAUDE.md.
    pub source: RuleSource,
    /// Confidence tier — drives which subsection the rule renders into.
    /// `Hard` rules are live invariants (incidents, reverts, security);
    /// `Strong` rules are high-enforcement team conventions; `Observed`
    /// rules are pattern reports the agent should weight as hints rather
    /// than law. Reader: the agent now treats Hard ≫ Strong ≫ Observed.
    pub confidence: RuleConfidence,
}

/// Three-tier confidence rating for a critical rule. Rendered as three
/// separate subsections inside `<critical_rules>` so the agent can
/// weight rules by evidence strength instead of treating every bullet
/// as equally binding. This addresses the ChatGPT review feedback that
/// mixed-strength bullets caused the agent to treat soft heuristics
/// like hard law.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RuleConfidence {
    /// Production incident, revert-grade, or security invariant.
    /// Examples: "never use CheckIsUserInRole in multitenant API paths
    /// — caused cross-tenant data leak in revert b4f76e01f".
    Hard,
    /// Strong team preference, demonstrated by high enforcement on
    /// review (e.g. CodeRabbit fix_rate ≥ 0.80 AND prs ≥ 5).
    Strong,
    /// Pattern observation with lower signal. The reader treats these
    /// as "check me before writing code that looks like this" — not
    /// "the build breaks if you do this". Default: we don't want a
    /// rule whose confidence wasn't set to accidentally render as
    /// Hard, so `Observed` is the "I wasn't told" tier.
    #[default]
    Observed,
}

/// Derive the confidence tier from a rule's source and (for CodeRabbit
/// rules) its empirical stats. Immune rules always rank Hard because
/// they represent behaviour the team actively reverted in production.
pub fn confidence_from_source(
    source: &RuleSource,
    fix_rate: Option<f32>,
    pr_count: Option<usize>,
) -> RuleConfidence {
    match source {
        // Live revert or production incident — Hard.
        RuleSource::Immune => RuleConfidence::Hard,
        // Manual repo rules were added with deliberation — treat as
        // Strong (not Hard, because the user might have been wrong).
        RuleSource::RepoRule => RuleConfidence::Strong,
        // Rules copied from an existing CLAUDE.md are hand-authored by
        // the user themselves; assume Strong. Evidence attached later
        // may upgrade to Hard via caller intervention.
        RuleSource::Existing => RuleConfidence::Strong,
        // CodeRabbit confidence scales with empirical enforcement.
        RuleSource::CodeRabbit => {
            let fr = fix_rate.unwrap_or(0.0);
            let prs = pr_count.unwrap_or(0);
            if fr >= 0.90 && prs >= 10 {
                RuleConfidence::Hard
            } else if fr >= 0.80 && prs >= 5 {
                RuleConfidence::Strong
            } else {
                RuleConfidence::Observed
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleSource {
    /// Extracted from a repo rule whose id starts with `immune_` — these
    /// represent files that were flagged by the immune system from a
    /// reverted commit.
    Immune,
    /// Extracted from a repo rule whose id starts with `cr_` — an
    /// auto-promoted CodeRabbit pattern (cluster with fix_rate ≥
    /// threshold across ≥ N PRs). Distinct from `RepoRule` so the
    /// agent can weight "this is what reviewers actually caught" vs
    /// "this is a manually-added repo rule".
    CodeRabbit,
    /// Repo rule not prefixed `immune_` or `cr_`. Generic anti-pattern
    /// guidance added by the user.
    RepoRule,
    /// Copied verbatim from the existing CLAUDE.md the user already
    /// authored. Human rules take priority over engram-derived ones on
    /// conflicts.
    Existing,
}

/// A CodeRabbit cluster surfaced from a `review_pattern` graph node.
/// Used for the per-language rule file render — the top-K clusters
/// per language are embedded so the agent sees team-learned patterns
/// even when the cluster didn't clear the repo-rule auto-promotion
/// threshold.
#[derive(Debug, Clone)]
pub struct CodeRabbitRule {
    /// The cluster's canonical rule text (bold title from the first
    /// member's CodeRabbit comment).
    pub rule_text: String,
    /// Fix rate across fixed / wontFix members. 0.0-1.0.
    pub fix_rate: f32,
    /// Number of distinct PRs the pattern appeared in.
    pub pr_count: usize,
    /// Optional commit sha from `✅ Addressed in commits`.
    pub fix_commit: Option<String>,
    /// Composite score used for sort order within a language bucket.
    /// Higher = more confident + more frequently seen.
    pub composite_score: f32,
}

/// Per-language coding conventions detected via
/// [`cognitive_service::static_analyze_file_style`].
#[derive(Debug, Clone)]
pub struct LanguageRules {
    pub language: String,
    /// Comma-separated globs (`**/*.vb` / `**/*.ts,**/*.tsx` / …).
    pub glob: String,
    pub bullets: Vec<String>,
    /// Top-N most central files used to derive the rules. Cited so the
    /// reader can audit.
    pub sample_files: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct StateSummary {
    pub total_state_keys: usize,
    pub session_keys: usize,
    pub viewstate_keys: usize,
    pub application_keys: usize,
    pub cross_page_chains: usize,
    /// Top-5 state keys by total read+write fan-in — cited so the reader
    /// sees the highest-traffic state surface at a glance.
    pub top_keys: Vec<(String, usize)>,
}

#[derive(Debug, Clone, Default)]
pub struct DbSummary {
    pub table_count: usize,
    /// `(table, incoming_reference_count)`, top-5.
    pub top_tables: Vec<(String, usize)>,
}

#[derive(Debug, Clone, Default)]
pub struct AuthSummary {
    pub mode: String,
    pub required_roles: Vec<String>,
    pub session_auth_patterns: usize,
}

/// Complete input to the renderers. Every optional field represents a
/// signal that may or may not be present on a given project — renderers
/// drop the corresponding section when the signal is empty.
#[derive(Debug, Clone, Default)]
pub struct ProjectSnapshot {
    pub project_name: String,
    /// One-line derived description: "VB.NET ASP.NET WebForms",
    /// "Rust MCP server", "Python Django app", …. Built from language +
    /// framework detection in the handler.
    pub role_description: String,
    pub languages: Vec<LanguageShare>,
    /// Build / test command pair detected from `Cargo.toml` /
    /// `package.json` / `.sln` / `Makefile`. Empty when not detected —
    /// no guessing.
    pub build_commands: Vec<String>,
    pub danger_zones: Vec<DangerZone>,
    pub critical_rules: Vec<CriticalRule>,
    pub per_language_rules: Vec<LanguageRules>,
    pub state_summary: Option<StateSummary>,
    pub db_summary: Option<DbSummary>,
    pub co_change_pairs: Vec<(String, String, u32)>,
    pub auth_summary: Option<AuthSummary>,
    pub frontend_warnings: Vec<String>,
    pub existing_claude_md: Option<String>,
    /// CodeRabbit review-pattern clusters indexed by language.
    /// Populated from `review_pattern` graph nodes (kind=pattern) with
    /// their metadata JSON parsed back out. Only the top-K per
    /// language are actually rendered — the full list is kept so the
    /// renderer has ranking freedom.
    pub coderabbit_rules_by_language: std::collections::HashMap<String, Vec<CodeRabbitRule>>,
}

// ── Language → glob mapping ──────────────────────────────────────────────────

/// Map a language identifier (as returned by the search-store's
/// language-breakdown count) to the glob pattern(s) the Claude Code rule
/// system uses for progressive disclosure.
///
/// Falls back to `**/*.{language}` for anything we don't explicitly
/// recognise — so a project in a language nobody thought to hardcode
/// still produces a usable rules file.
pub fn language_to_globs(language: &str) -> String {
    match language.to_ascii_lowercase().as_str() {
        "vb" | "vbnet" | "vb.net" => "**/*.vb".into(),
        "cs" | "csharp" | "c#" => "**/*.cs".into(),
        "rust" | "rs" => "**/*.rs".into(),
        "python" | "py" => "**/*.py".into(),
        "typescript" | "ts" => "**/*.ts,**/*.tsx".into(),
        "javascript" | "js" => "**/*.js,**/*.jsx".into(),
        "java" => "**/*.java".into(),
        "go" => "**/*.go".into(),
        "cpp" | "c++" | "cxx" => "**/*.cpp,**/*.hpp,**/*.cc,**/*.h".into(),
        "c" | "ansi_c" => "**/*.c,**/*.h".into(),
        "ruby" | "rb" => "**/*.rb".into(),
        "sql" => "**/*.sql".into(),
        "kotlin" | "kt" => "**/*.kt,**/*.kts".into(),
        "swift" => "**/*.swift".into(),
        "scala" => "**/*.scala,**/*.sc".into(),
        "php" => "**/*.php".into(),
        "html" | "aspx" => "**/*.aspx,**/*.ascx,**/*.html".into(),
        other => format!("**/*.{other}"),
    }
}

/// Human-friendly display name for the language label — fed into the
/// `<role>` summary and rule-file headers. Returns the input unchanged
/// when we don't have a canonical display form.
pub fn language_display(language: &str) -> &str {
    match language.to_ascii_lowercase().as_str() {
        "vb" | "vbnet" | "vb.net" => "VB.NET",
        "cs" | "csharp" | "c#" => "C#",
        "rust" | "rs" => "Rust",
        "python" | "py" => "Python",
        "typescript" | "ts" => "TypeScript",
        "javascript" | "js" => "JavaScript",
        "java" => "Java",
        "go" => "Go",
        "cpp" | "c++" | "cxx" => "C++",
        "c" | "ansi_c" => "C",
        "ruby" | "rb" => "Ruby",
        "sql" => "SQL",
        "kotlin" | "kt" => "Kotlin",
        "swift" => "Swift",
        "scala" => "Scala",
        "php" => "PHP",
        _ => language,
    }
}

/// Slug-ify a language label for a filename.
/// `VB.NET` → `vbnet-conventions.md`, `C++` → `cpp-conventions.md`.
pub fn language_slug(language: &str) -> String {
    let mut out = String::with_capacity(language.len());
    for c in language.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        }
    }
    if out.is_empty() {
        "unknown".to_string()
    } else {
        out
    }
}

// ── Rule-file registry ───────────────────────────────────────────────────────

/// A generated rule file, keyed by filename inside `.claude/rules/`.
#[derive(Debug, Clone)]
pub struct RuleFile {
    pub filename: String,
    pub content: String,
}

// ── Root CLAUDE.md rendering ─────────────────────────────────────────────────

const HARD_MAX_ROOT_LINES: usize = 300;

/// Render the root `CLAUDE.md` from the snapshot. Keeps the output under
/// `max_lines` (floored at 20, capped at [`HARD_MAX_ROOT_LINES`]) by
/// trimming lower-priority sections when the budget is tight. The
/// critical-rules / danger-zones blocks are always preserved — they're
/// the point of the file.
pub fn render_root_claude_md(snapshot: &ProjectSnapshot, max_lines: usize) -> String {
    let max = max_lines.clamp(20, HARD_MAX_ROOT_LINES);
    let rule_files = planned_rule_file_metadata(snapshot);

    let mut out = String::with_capacity(2048);
    let _ = writeln!(out, "# {}", display_name(&snapshot.project_name));
    out.push('\n');

    // <role>
    if !snapshot.role_description.is_empty() {
        out.push_str("<role>\n");
        let _ = writeln!(out, "{}", snapshot.role_description.trim());
        out.push_str("</role>\n\n");
    }

    // <critical_rules> — highest-priority section, never dropped.
    // Rules are now bucketed by confidence tier so the agent can weight
    // hard invariants above team preferences above observed patterns:
    //   🛡 Hard         — live incidents, reverts, security invariants
    //   ⚠️ Strong       — team conventions enforced on review
    //   📊 Observed     — lower-signal review observations (advisory)
    // Each rule keeps its provenance tag (🛡 [immune] / 🐰 [CodeRabbit])
    // so the agent can also see where the rule came from.
    if !snapshot.critical_rules.is_empty() {
        out.push_str("<critical_rules>\n");
        let (hard, strong, observed) = partition_rules_by_confidence(&snapshot.critical_rules);
        render_rule_bucket(
            &mut out,
            "🛡 Hard rules — live incidents, reverts, security invariants",
            &hard,
        );
        render_rule_bucket(
            &mut out,
            "⚠️ Strong conventions — team-enforced on review",
            &strong,
        );
        render_rule_bucket(
            &mut out,
            "📊 Observed patterns — common reviewer feedback (advisory)",
            &observed,
        );
        out.push_str("</critical_rules>\n\n");
    }

    // <build>
    if !snapshot.build_commands.is_empty() {
        out.push_str("<build>\n");
        for cmd in &snapshot.build_commands {
            let _ = writeln!(out, "{}", cmd.trim());
        }
        out.push_str("</build>\n\n");
    }

    // <danger_zones> — keep top-5.
    if !snapshot.danger_zones.is_empty() {
        out.push_str("<danger_zones>\n");
        for z in snapshot.danger_zones.iter().take(5) {
            let reasons = if z.reasons.is_empty() {
                String::new()
            } else {
                format!(", {}", z.reasons.join(", "))
            };
            let _ = writeln!(
                out,
                "- {} ({}/10 {}, {} downstream{})",
                z.file_path, z.risk_score, z.risk_band, z.total_downstream, reasons
            );
        }
        out.push_str("</danger_zones>\n\n");
    }

    // <conventions> — pointers to the .claude/rules/ files we generated.
    if !rule_files.is_empty() {
        out.push_str("<conventions>\n");
        out.push_str("See .claude/rules/ for language-specific conventions:\n");
        for (fname, glob) in &rule_files {
            if glob.is_empty() {
                let _ = writeln!(out, "- .claude/rules/{fname}");
            } else {
                let _ = writeln!(out, "- .claude/rules/{fname} (globs: {glob})");
            }
        }
        out.push_str("</conventions>\n\n");
    }

    // <engram> — always present; it's the user manual for our own tools.
    out.push_str("<engram>\n");
    out.push_str("This project is indexed by Engram MCP. Key tools:\n");
    out.push_str("- `immune_check` before editing danger-zone files\n");
    out.push_str("- `compute_blast_radius` before large refactors\n");
    out.push_str("- `analyze_file_coding_style` before writing code in unfamiliar files\n");
    out.push_str("- `trace_data_flow` / `trace_ui_event` to trace data + UI paths\n");
    out.push_str("</engram>\n");

    // Trim tail if over budget. We drop from `<engram>` backwards toward
    // `<conventions>` — critical_rules + danger_zones stay put.
    enforce_root_budget(out, max)
}

/// Pair each language / signal-driven rule file with its glob so the
/// root `<conventions>` block can reference them.
fn planned_rule_file_metadata(snapshot: &ProjectSnapshot) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for lang in &snapshot.per_language_rules {
        if lang.bullets.is_empty() {
            continue;
        }
        let filename = format!("{}-conventions.md", language_slug(&lang.language));
        out.push((filename, lang.glob.clone()));
    }
    if !snapshot.danger_zones.is_empty() {
        out.push(("danger-zones.md".into(), String::new()));
    }
    if snapshot.state_summary.is_some() || snapshot.db_summary.is_some() {
        out.push(("state-and-data.md".into(), String::new()));
    }
    if !snapshot.co_change_pairs.is_empty() {
        out.push(("co-change-pairs.md".into(), String::new()));
    }
    out
}

/// Trim whole sections from the end of the root output until it fits
/// within `max_lines`, but never drop `<critical_rules>` or the header.
fn enforce_root_budget(text: String, max_lines: usize) -> String {
    if text.lines().count() <= max_lines {
        return text;
    }
    // Priority from lowest → highest drop order.
    let drop_order = ["<engram>", "<conventions>", "<build>"];
    let mut out = text;
    for tag in drop_order {
        if out.lines().count() <= max_lines {
            return out;
        }
        out = strip_section(&out, tag);
    }
    // If still over budget, truncate with a clear marker so the caller
    // sees what happened (should basically never happen at default 60).
    if out.lines().count() > max_lines {
        let kept: Vec<&str> = out.lines().take(max_lines - 1).collect();
        let mut truncated = kept.join("\n");
        truncated.push('\n');
        truncated.push_str("<!-- CLAUDE.md truncated: max_root_lines exceeded; see .claude/rules/ for full content -->\n");
        return truncated;
    }
    out
}

/// Remove the `<tag>`…`</tag>` block (inclusive of the tags + trailing
/// blank line) from `text`. No-op if the tag isn't present.
///
/// `tag` is expected to be the literal opening form including angle
/// brackets (e.g. `"<engram>"`). The closing tag is derived by
/// stripping the brackets and wrapping in `</…>`.
fn strip_section(text: &str, tag: &str) -> String {
    let open = format!("{tag}\n");
    let close_name = tag.trim_start_matches('<').trim_end_matches('>');
    let close = format!("</{close_name}>\n");

    let Some(start) = text.find(&open) else {
        return text.to_string();
    };
    let close_start = match text[start..].find(&close) {
        Some(i) => start + i + close.len(),
        None => return text.to_string(),
    };
    // Also swallow the blank line that usually follows.
    let end = if text[close_start..].starts_with('\n') {
        close_start + 1
    } else {
        close_start
    };
    let mut out = String::with_capacity(text.len() - (end - start));
    out.push_str(&text[..start]);
    out.push_str(&text[end..]);
    out
}

fn display_name(raw: &str) -> String {
    if raw.is_empty() {
        "Project".into()
    } else {
        raw.to_string()
    }
}

// ── Rule-file rendering ──────────────────────────────────────────────────────

/// Build every `.claude/rules/*.md` file the snapshot justifies. Keyed by
/// filename so the handler can iterate + write them to disk in one pass.
pub fn render_rule_files(snapshot: &ProjectSnapshot) -> Vec<RuleFile> {
    let mut files = Vec::new();

    // One conventions file per language that produced bullets. We
    // also include a file when a language has NO deterministic style
    // bullets but DOES have CodeRabbit patterns — the team-learned
    // rules are worth surfacing on their own.
    let mut emitted_langs: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    for lang in &snapshot.per_language_rules {
        let cr_rules = snapshot
            .coderabbit_rules_by_language
            .get(&lang.language)
            .cloned()
            .unwrap_or_default();
        let mut ranked = cr_rules;
        ranked.sort_by(|a, b| {
            b.composite_score
                .partial_cmp(&a.composite_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        if lang.bullets.is_empty() && ranked.is_empty() {
            continue;
        }
        files.push(RuleFile {
            filename: format!("{}-conventions.md", language_slug(&lang.language)),
            content: render_language_rules_with_cr(lang, &ranked),
        });
        emitted_langs.insert(lang.language.clone());
    }

    // Languages present in coderabbit_rules_by_language but without a
    // per_language_rules entry still deserve a conventions file — the
    // CR patterns alone are useful.
    for (language, cr_rules) in &snapshot.coderabbit_rules_by_language {
        if emitted_langs.contains(language) || cr_rules.is_empty() {
            continue;
        }
        let synthetic_lang = LanguageRules {
            language: language.clone(),
            glob: language_to_globs(language),
            bullets: Vec::new(),
            sample_files: Vec::new(),
        };
        let mut ranked = cr_rules.clone();
        ranked.sort_by(|a, b| {
            b.composite_score
                .partial_cmp(&a.composite_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        files.push(RuleFile {
            filename: format!("{}-conventions.md", language_slug(language)),
            content: render_language_rules_with_cr(&synthetic_lang, &ranked),
        });
    }

    if !snapshot.danger_zones.is_empty() {
        files.push(RuleFile {
            filename: "danger-zones.md".into(),
            content: render_danger_zones(&snapshot.danger_zones),
        });
    }

    if snapshot.state_summary.is_some() || snapshot.db_summary.is_some() {
        files.push(RuleFile {
            filename: "state-and-data.md".into(),
            content: render_state_and_data(
                snapshot.state_summary.as_ref(),
                snapshot.db_summary.as_ref(),
                snapshot.auth_summary.as_ref(),
            ),
        });
    }

    if !snapshot.co_change_pairs.is_empty() {
        files.push(RuleFile {
            filename: "co-change-pairs.md".into(),
            content: render_co_change_pairs(&snapshot.co_change_pairs),
        });
    }

    if !snapshot.frontend_warnings.is_empty() {
        files.push(RuleFile {
            filename: "frontend-notes.md".into(),
            content: render_frontend_notes(&snapshot.frontend_warnings),
        });
    }

    files
}

/// Visual marker placed at the start of each critical-rules bullet so
/// the agent can weight rules by provenance at a glance. The marker
/// is a short prefix — kept compact so it doesn't eat into the
/// attention budget of the rule itself.
fn rule_source_tag(src: &RuleSource) -> &'static str {
    match src {
        RuleSource::Immune => "🛡 [immune] ",
        RuleSource::CodeRabbit => "🐰 [CodeRabbit] ",
        RuleSource::RepoRule => "",
        RuleSource::Existing => "",
    }
}

/// Split a flat critical-rules vector into `(hard, strong, observed)`
/// buckets. Preserves the caller's sort order within each bucket so
/// the render output is stable.
fn partition_rules_by_confidence(
    rules: &[CriticalRule],
) -> (Vec<&CriticalRule>, Vec<&CriticalRule>, Vec<&CriticalRule>) {
    let mut hard = Vec::new();
    let mut strong = Vec::new();
    let mut observed = Vec::new();
    for r in rules {
        match r.confidence {
            RuleConfidence::Hard => hard.push(r),
            RuleConfidence::Strong => strong.push(r),
            RuleConfidence::Observed => observed.push(r),
        }
    }
    (hard, strong, observed)
}

/// Render one confidence-tier subsection into `<critical_rules>`.
/// Skipped entirely when the bucket is empty — no stray headings with
/// no bullets underneath.
fn render_rule_bucket(out: &mut String, heading: &str, bucket: &[&CriticalRule]) {
    if bucket.is_empty() {
        return;
    }
    let _ = writeln!(out, "\n### {heading}\n");
    for rule in bucket {
        let text = rule.text.trim();
        let tag = rule_source_tag(&rule.source);
        match &rule.evidence {
            Some(ev) if !ev.is_empty() => {
                let _ = writeln!(out, "- {tag}{text} {ev}");
            }
            _ => {
                let _ = writeln!(out, "- {tag}{text}");
            }
        }
    }
}

/// Top-K limit on CodeRabbit clusters embedded in each language rule
/// file. Tuned to fit within the attention budget — ~10 rules ≈ 15-20
/// lines of output including the header. If a project has hundreds of
/// high-signal clusters per language, only the top by composite score
/// survive.
const CODERABBIT_PER_LANGUAGE_CAP: usize = 10;

/// Render a single `LanguageRules` bucket as the `.claude/rules/…md`
/// content. `cr_rules` is the pre-sorted, language-scoped list of
/// CodeRabbit patterns to append — empty when the project hasn't
/// ingested CodeRabbit history or when no pattern matched this
/// language.
fn render_language_rules_with_cr(lang: &LanguageRules, cr_rules: &[CodeRabbitRule]) -> String {
    let mut out = String::with_capacity(512);
    let _ = writeln!(out, "---");
    let _ = writeln!(out, "globs: \"{}\"", lang.glob);
    let _ = writeln!(out, "---");
    out.push('\n');
    let _ = writeln!(out, "# {} conventions", language_display(&lang.language));
    out.push('\n');
    if !lang.sample_files.is_empty() {
        let _ = writeln!(out, "_Detected from: {}_", lang.sample_files.join(", "));
        out.push('\n');
    }
    out.push_str("<instructions>\n");
    for b in &lang.bullets {
        let b = b.trim();
        if b.is_empty() {
            continue;
        }
        if b.starts_with('-') || b.starts_with('*') {
            let _ = writeln!(out, "{b}");
        } else {
            let _ = writeln!(out, "- {b}");
        }
    }
    out.push_str("</instructions>\n");

    // CodeRabbit patterns section — only when there are any for this
    // language. Sorted by composite score (fix_rate × log₂(pr_count + 1))
    // and capped at CODERABBIT_PER_LANGUAGE_CAP so the file stays
    // readable. We sort here (not only at the call site) so this
    // renderer is robust regardless of the order its caller supplies.
    if !cr_rules.is_empty() {
        let mut ranked: Vec<&CodeRabbitRule> = cr_rules.iter().collect();
        ranked.sort_by(|a, b| {
            b.composite_score
                .partial_cmp(&a.composite_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        out.push('\n');
        let _ = writeln!(
            out,
            "## CodeRabbit patterns (top {})",
            ranked.len().min(CODERABBIT_PER_LANGUAGE_CAP)
        );
        out.push('\n');
        out.push_str(
            "Patterns CodeRabbit flagged that the team **fixed across multiple PRs**. \
             Each line is a class of issue reviewers catch repeatedly in this \
             language — check the added code against these before proposing.\n\n",
        );
        for rule in ranked.iter().take(CODERABBIT_PER_LANGUAGE_CAP) {
            let sha = rule
                .fix_commit
                .as_deref()
                .map(|s| format!(", fix {s}"))
                .unwrap_or_default();
            let _ = writeln!(
                out,
                "- **{text}** — {prs} PR{plural}, {rate:.0}% fix rate{sha}",
                text = rule.rule_text.trim(),
                prs = rule.pr_count,
                plural = if rule.pr_count == 1 { "" } else { "s" },
                rate = rule.fix_rate * 100.0,
                sha = sha,
            );
        }
    }
    out
}

fn render_danger_zones(zones: &[DangerZone]) -> String {
    let mut out = String::with_capacity(512);
    out.push_str("---\n");
    out.push_str(
        "# No glob — read for any change. Pair with `immune_check` before editing these files.\n",
    );
    out.push_str("---\n\n");
    out.push_str("# Danger zones\n\n");
    out.push_str(
        "High-blast-radius files detected by `compute_blast_radius`. \
         Changes to these files cascade widely — run `immune_check` with the \
         proposed snippet and set `file_path` to the target before committing.\n\n",
    );
    for z in zones {
        let reasons = if z.reasons.is_empty() {
            String::new()
        } else {
            format!(" — {}", z.reasons.join(", "))
        };
        let _ = writeln!(
            out,
            "- **{}** ({}/10 {}, {} downstream){}",
            z.file_path, z.risk_score, z.risk_band, z.total_downstream, reasons
        );
    }
    out
}

fn render_state_and_data(
    state: Option<&StateSummary>,
    db: Option<&DbSummary>,
    auth: Option<&AuthSummary>,
) -> String {
    let mut out = String::with_capacity(512);
    out.push_str("---\n");
    out.push_str("# State management + database surface.\n");
    out.push_str("---\n\n");
    out.push_str("# State and data\n\n");

    if let Some(s) = state {
        out.push_str("## Session / ViewState / Application state\n\n");
        let _ = writeln!(
            out,
            "- Total distinct state keys: **{}** ({} Session, {} ViewState, {} Application)",
            s.total_state_keys, s.session_keys, s.viewstate_keys, s.application_keys
        );
        if s.cross_page_chains > 0 {
            let _ = writeln!(
                out,
                "- Cross-page state chains: **{}** (a state key touched by 2+ pages)",
                s.cross_page_chains
            );
        }
        if !s.top_keys.is_empty() {
            out.push_str("- Highest-traffic keys (reads + writes):\n");
            for (k, n) in s.top_keys.iter().take(5) {
                let _ = writeln!(out, "  - `{k}` ({n} ops)");
            }
        }
        out.push('\n');
    }

    if let Some(d) = db {
        out.push_str("## Database tables\n\n");
        let _ = writeln!(out, "- Tables in graph: **{}**", d.table_count);
        if !d.top_tables.is_empty() {
            out.push_str("- Most referenced tables:\n");
            for (t, n) in d.top_tables.iter().take(5) {
                let _ = writeln!(out, "  - `{t}` ({n} refs)");
            }
        }
        out.push('\n');
    }

    if let Some(a) = auth {
        out.push_str("## Auth\n\n");
        if !a.mode.is_empty() {
            let _ = writeln!(out, "- Mode: **{}**", a.mode);
        }
        if !a.required_roles.is_empty() {
            let _ = writeln!(out, "- Required roles: {}", a.required_roles.join(", "));
        }
        if a.session_auth_patterns > 0 {
            let _ = writeln!(
                out,
                "- Session-based auth patterns: {}",
                a.session_auth_patterns
            );
        }
        out.push('\n');
    }

    out
}

fn render_co_change_pairs(pairs: &[(String, String, u32)]) -> String {
    let mut out = String::with_capacity(512);
    out.push_str("---\n");
    out.push_str(
        "# Temporal coupling — files that historically change together in the same commit.\n",
    );
    out.push_str(
        "# If you touch one side, expect the other to need an update — verify in the linked file.\n",
    );
    out.push_str("---\n\n");
    out.push_str("# Co-change pairs\n\n");
    out.push_str("Top pairs by co-change frequency (from git history):\n\n");
    for (a, b, n) in pairs.iter().take(20) {
        let _ = writeln!(out, "- `{a}` ↔ `{b}` ({n} co-changes)");
    }
    out
}

fn render_frontend_notes(warnings: &[String]) -> String {
    let mut out = String::with_capacity(256);
    out.push_str("---\n");
    out.push_str("globs: \"**/*.js,**/*.jsx,**/*.ts,**/*.tsx\"\n");
    out.push_str("---\n\n");
    out.push_str("# Frontend notes\n\n");
    out.push_str("<instructions>\n");
    for w in warnings {
        let w = w.trim();
        if w.is_empty() {
            continue;
        }
        if w.starts_with('-') {
            let _ = writeln!(out, "{w}");
        } else {
            let _ = writeln!(out, "- {w}");
        }
    }
    out.push_str("</instructions>\n");
    out
}

// ── AGENTS.md rendering ──────────────────────────────────────────────────────

/// Generate an interoperable `AGENTS.md` variant that non-Claude agents
/// (Copilot, Cursor, Codex) can read. Shares the same data as the root
/// `CLAUDE.md` but drops the Claude-specific `<engram>` call-out and the
/// glob pointers, inlining key rules instead.
pub fn render_agents_md(snapshot: &ProjectSnapshot, max_lines: usize) -> String {
    let mut out = String::with_capacity(2048);
    let _ = writeln!(
        out,
        "# {} — agent notes",
        display_name(&snapshot.project_name)
    );
    out.push('\n');
    if !snapshot.role_description.is_empty() {
        let _ = writeln!(out, "{}", snapshot.role_description.trim());
        out.push('\n');
    }

    if !snapshot.critical_rules.is_empty() {
        out.push_str("## Critical rules\n\n");
        for r in &snapshot.critical_rules {
            match &r.evidence {
                Some(ev) if !ev.is_empty() => {
                    let _ = writeln!(out, "- {} {}", r.text.trim(), ev);
                }
                _ => {
                    let _ = writeln!(out, "- {}", r.text.trim());
                }
            }
        }
        out.push('\n');
    }

    if !snapshot.build_commands.is_empty() {
        out.push_str("## Build / test\n\n");
        out.push_str("```\n");
        for cmd in &snapshot.build_commands {
            let _ = writeln!(out, "{}", cmd.trim());
        }
        out.push_str("```\n\n");
    }

    if !snapshot.danger_zones.is_empty() {
        out.push_str("## Danger zones\n\n");
        for z in snapshot.danger_zones.iter().take(5) {
            let _ = writeln!(
                out,
                "- {} ({}/10 {}, {} downstream)",
                z.file_path, z.risk_score, z.risk_band, z.total_downstream
            );
        }
        out.push('\n');
    }

    // AGENTS.md gets a compact summary of language conventions inline
    // rather than a pointer to a Claude-rules directory.
    if !snapshot.per_language_rules.is_empty() {
        out.push_str("## Language conventions\n\n");
        for lang in &snapshot.per_language_rules {
            if lang.bullets.is_empty() {
                continue;
            }
            let _ = writeln!(
                out,
                "### {} (globs: {})",
                language_display(&lang.language),
                lang.glob
            );
            for b in &lang.bullets {
                let b = b.trim();
                if b.starts_with('-') {
                    let _ = writeln!(out, "{b}");
                } else {
                    let _ = writeln!(out, "- {b}");
                }
            }
            out.push('\n');
        }
    }

    // Cap to max_lines with the same trimming priority as the root.
    if out.lines().count() > max_lines {
        let kept: Vec<&str> = out.lines().take(max_lines).collect();
        return kept.join("\n") + "\n";
    }
    out
}

// ── Existing-CLAUDE.md merge helpers ─────────────────────────────────────────

/// Parse a pre-existing CLAUDE.md for rules the human explicitly marked as
/// critical. We look for bullets inside a `<critical_rules>` XML block
/// first (the shape this tool emits) and fall back to bullets under any
/// heading whose text contains "critical", "rule", or "mandatory".
///
/// Returns the rule text only — evidence stays `None` (the human wrote it,
/// the engram-derived evidence attaches later).
pub fn extract_critical_rules_from_existing(md: &str) -> Vec<CriticalRule> {
    let mut out = Vec::new();

    // Prefer the structured <critical_rules>…</critical_rules> block.
    if let Some(start) = md.find("<critical_rules>") {
        let after = &md[start + "<critical_rules>".len()..];
        if let Some(end) = after.find("</critical_rules>") {
            for line in after[..end].lines() {
                if let Some(rule) = extract_bullet(line) {
                    // Skip engram-generated rules from a prior run —
                    // otherwise they get re-promoted into the next run
                    // and the critical_rules section bloats over time.
                    // The pipeline will re-generate them from source
                    // data this run.
                    if is_engram_generated_rule(&rule) {
                        continue;
                    }
                    out.push(CriticalRule {
                        text: rule,
                        evidence: None,
                        source: RuleSource::Existing,
                        confidence: Default::default(),
                    });
                }
            }
            if !out.is_empty() {
                return out;
            }
        }
    }

    // Fallback: scan for headings containing "critical" / "rule" /
    // "mandatory" and collect the bullets that follow until the next
    // heading.
    let mut in_block = false;
    for line in md.lines() {
        let trimmed = line.trim_start_matches('#').trim().to_lowercase();
        if line.starts_with('#') {
            in_block = trimmed.contains("critical")
                || trimmed.contains("mandatory")
                || trimmed.contains("hard-learned");
            continue;
        }
        if !in_block {
            continue;
        }
        if let Some(rule) = extract_bullet(line) {
            if is_engram_generated_rule(&rule) {
                continue;
            }
            out.push(CriticalRule {
                text: rule,
                evidence: None,
                source: RuleSource::Existing,
                confidence: Default::default(),
            });
        }
    }
    out
}

/// True when a rule's text carries the unmistakable fingerprint of a
/// previous engram run — the `[LLM-curated from …]` suffix, a `cr_…`
/// or `immune_…` id in parens, the canonical "CodeRabbit pattern, N
/// PRs, M% fix rate" footer, or the section-heading emojis we emit.
/// Used by `extract_critical_rules_from_existing` to drop stale rows
/// so the next run doesn't accumulate layers of self-ingestion.
fn is_engram_generated_rule(text: &str) -> bool {
    let t = text.trim();
    // Strip the common source tag prefix so the markers fire on the
    // payload regardless of the leading "🛡 [immune]" / "🐰 [CodeRabbit]".
    let body = t
        .trim_start_matches("🛡 [immune] ")
        .trim_start_matches("🐰 [CodeRabbit] ")
        .trim_start_matches("🛡 ")
        .trim_start_matches("🐰 ");
    body.contains("[LLM-curated from")
        || body.contains("CodeRabbit pattern,")
        || body.contains("(cr_")
        || body.contains("(immune_")
        || body.starts_with("AVOID (reverted in")
}

fn extract_bullet(line: &str) -> Option<String> {
    let trimmed = line.trim();
    for prefix in ["- ", "* ", "• "] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let rest = rest.trim();
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
    }
    None
}

/// Marker pair used by `splice_engram_section`. Everything between
/// these two lines in a CLAUDE.md is considered engram-owned and is
/// safe to replace on a rerun. Content outside the markers is
/// hand-authored and MUST be preserved verbatim.
pub const ENGRAM_BEGIN_MARKER: &str = "<!-- engram:begin -->";
pub const ENGRAM_END_MARKER: &str = "<!-- engram:end -->";

/// Splice an engram-generated block into an existing CLAUDE.md,
/// preserving every byte outside the engram-owned region.
///
/// Semantics:
/// - If `existing` already contains both markers, replace the span
///   between them (inclusive) with the new engram block.
/// - If `existing` does NOT contain the markers, append the new
///   engram block to the end of `existing` under a clear heading.
///   Hand-authored content above stays exactly where it is.
/// - `engram_block` is the full rendered engram output — the
///   splicer wraps it in `<!-- engram:begin --> ... <!-- engram:end -->`
///   so the next rerun can find it precisely.
///
/// This is what `write_to_disk: true` with an existing CLAUDE.md
/// should have been doing from day one. Fixes the bug where a
/// hand-authored CLAUDE.md got replaced by the ~60-line engram
/// output.
pub fn splice_engram_section(existing: &str, engram_block: &str) -> String {
    let block_body = engram_block.trim_end_matches('\n');
    let wrapped = format!(
        "{ENGRAM_BEGIN_MARKER}\n\
         _This section is managed by engram. Edits between the markers \
         are overwritten on the next `produce_claude_md` run._\n\n\
         {block_body}\n\n\
         {ENGRAM_END_MARKER}"
    );

    // Case 1: both markers present → replace the span.
    if let (Some(b), Some(e)) = (
        existing.find(ENGRAM_BEGIN_MARKER),
        existing.find(ENGRAM_END_MARKER),
    ) {
        if b < e {
            let mut out = String::with_capacity(existing.len() + wrapped.len());
            out.push_str(&existing[..b]);
            out.push_str(&wrapped);
            let end_after = e + ENGRAM_END_MARKER.len();
            out.push_str(&existing[end_after..]);
            return out;
        }
    }

    // Case 2: no markers → append at the end under a clear heading.
    // Hand-authored content above is untouched; the section heading
    // signals that everything below is engram-owned.
    let mut out = String::with_capacity(existing.len() + wrapped.len() + 64);
    out.push_str(existing.trim_end());
    out.push_str("\n\n---\n\n");
    out.push_str("## Engram-generated guidance\n\n");
    out.push_str(&wrapped);
    out.push('\n');
    out
}

/// Result of an optimize-rewrite operation. The report is surfaced
/// alongside the rewritten markdown so the caller sees exactly which
/// sections engram took ownership of vs which human content survived.
#[derive(Debug, Clone, Default)]
pub struct OptimizeReport {
    /// Section headings engram replaced (owned-by-engram categories).
    pub replaced_sections: Vec<String>,
    /// Section headings preserved verbatim from the existing file
    /// (domain context, architecture notes, onboarding, etc.).
    pub preserved_sections: Vec<String>,
    /// Approximate line-count of the existing file before rewrite.
    pub original_line_count: usize,
    /// Line count of the rewritten result.
    pub rewritten_line_count: usize,
}

/// Section-level rewrite: replace engram-owned sections in the
/// existing CLAUDE.md with fresh engram output; preserve
/// human-authored sections (architecture notes, onboarding, domain
/// context) exactly. Returns the merged markdown + a report.
///
/// Ownership rule: a heading is considered **engram-owned** when its
/// normalised text matches one of the engram-generated section
/// names. These sections mechanically describe the codebase —
/// engram's version is evidence-backed and current, so replacing
/// human-authored copies is correct.
///
/// Everything else — numbered manuals, "never do X because Y"
/// rationales, onboarding steps, custom section names — is
/// preserved verbatim because it represents insight engram cannot
/// produce from the graph.
pub fn optimize_rewrite(
    existing: &str,
    engram_block: &str,
) -> (String, OptimizeReport) {
    let original_line_count = existing.lines().count();
    let sections = split_into_sections(existing);

    let mut preserved_body = String::with_capacity(existing.len());
    let mut report = OptimizeReport {
        original_line_count,
        ..Default::default()
    };
    for s in sections {
        if is_engram_owned_heading(&s.heading) {
            report.replaced_sections.push(s.heading.clone());
            continue;
        }
        if !preserved_body.is_empty() && !preserved_body.ends_with("\n\n") {
            preserved_body.push('\n');
        }
        preserved_body.push_str(&s.raw);
        if !report.preserved_sections.contains(&s.heading) {
            report.preserved_sections.push(s.heading.clone());
        }
    }

    // Compose: engram block at the TOP (highest priority attention
    // budget), followed by preserved human content. A human reader
    // of CLAUDE.md sees the fresh engram-generated guidance first,
    // then the domain-specific context below.
    let mut out = String::with_capacity(engram_block.len() + preserved_body.len() + 128);
    out.push_str(ENGRAM_BEGIN_MARKER);
    out.push('\n');
    out.push_str(
        "_This section is managed by engram. Edits between the markers are overwritten on \
         the next `produce_claude_md` run._\n\n",
    );
    out.push_str(engram_block.trim_end_matches('\n'));
    out.push_str("\n\n");
    out.push_str(ENGRAM_END_MARKER);
    out.push_str("\n\n");
    if !preserved_body.trim().is_empty() {
        out.push_str("---\n\n");
        out.push_str("## Project-specific guidance (preserved)\n\n");
        out.push_str(preserved_body.trim());
        out.push('\n');
    }

    report.rewritten_line_count = out.lines().count();
    (out, report)
}

/// Small helper: one section of a markdown document — the heading
/// line plus everything until the next heading of equal or higher
/// level.
struct Section {
    heading: String,
    raw: String,
}

fn split_into_sections(md: &str) -> Vec<Section> {
    // Walk lines; open a new section on every `#`-prefixed line.
    // Content before the first heading goes into a synthetic
    // "(preamble)" section so it isn't lost.
    let mut out: Vec<Section> = Vec::new();
    let mut current = Section {
        heading: "(preamble)".into(),
        raw: String::new(),
    };
    for line in md.lines() {
        if line.starts_with('#') {
            if !current.raw.trim().is_empty() || current.heading != "(preamble)" {
                out.push(current);
            }
            // Heading line becomes part of the new section's raw.
            let heading = line.trim_start_matches('#').trim().to_string();
            current = Section {
                heading,
                raw: String::new(),
            };
            current.raw.push_str(line);
            current.raw.push('\n');
        } else {
            current.raw.push_str(line);
            current.raw.push('\n');
        }
    }
    if !current.raw.trim().is_empty() || !out.is_empty() {
        out.push(current);
    }
    out
}

/// Return true when a section heading describes content engram
/// generates deterministically from the graph. Normalised match so
/// "Critical Rules", "CRITICAL RULES", "Critical rules (from graph)"
/// all collapse to the same slot.
fn is_engram_owned_heading(heading: &str) -> bool {
    let h = heading.trim().to_ascii_lowercase();
    // Leading bracket tags like "[engram] …" also count.
    let core = h
        .trim_start_matches('[')
        .splitn(2, ']')
        .last()
        .unwrap_or(&h)
        .trim()
        .to_string();
    const OWNED: &[&str] = &[
        "critical rules",
        "critical_rules",
        "danger zones",
        "danger_zones",
        "blast radius",
        "conventions",
        "per-language conventions",
        "language conventions",
        "temporal couplings",
        "co-change pairs",
        "state summary",
        "database summary",
        "engram",
        "engram tools",
        "engram-generated guidance",
        "role",
        "build",
        "coderabbit patterns",
    ];
    OWNED.iter().any(|name| {
        core == *name
            || core.starts_with(&format!("{name} "))
            || core.starts_with(&format!("{name}:"))
    })
}

/// Merge engram-derived rules with rules extracted from an existing
/// CLAUDE.md. Human rules take priority on conflict — if the human
/// wrote a rule whose text is a prefix of an engram-derived rule (or
/// vice versa), the human text wins and any engram-derived evidence is
/// appended as the `evidence` suffix.
pub fn merge_with_existing(
    engram_derived: Vec<CriticalRule>,
    existing: Vec<CriticalRule>,
) -> Vec<CriticalRule> {
    fn key(text: &str) -> String {
        text.to_lowercase()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || c.is_ascii_whitespace())
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }
    let mut out = Vec::with_capacity(engram_derived.len() + existing.len());
    let mut seen: Vec<String> = Vec::new();

    // Existing (human) rules first — they set the order.
    for mut r in existing {
        let k = key(&r.text);
        // Look for engram-derived evidence we can attach.
        if let Some(match_) = engram_derived
            .iter()
            .find(|e| key(&e.text).contains(&k) || k.contains(&key(&e.text)))
        {
            if r.evidence.is_none() {
                r.evidence = match_.evidence.clone();
            }
        }
        seen.push(k);
        out.push(r);
    }

    // Then engram-derived rules that don't duplicate a human rule.
    for r in engram_derived {
        let k = key(&r.text);
        if seen.iter().any(|s| s.contains(&k) || k.contains(s)) {
            continue;
        }
        seen.push(k);
        out.push(r);
    }

    out
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(text: &str, source: RuleSource, confidence: RuleConfidence) -> CriticalRule {
        CriticalRule {
            text: text.into(),
            evidence: None,
            source,
            confidence,
        }
    }

    #[test]
    fn confidence_tier_rendering_buckets_rules_with_subheadings() {
        // Covers ChatGPT review item #1: mixed-strength bullets were
        // blended into one flat list. This test pins the new behaviour:
        // three clearly-separated subsections with the strongest-rule
        // heading first, and empty-bucket headings are omitted.
        let snapshot = ProjectSnapshot {
            project_name: "Demo".into(),
            role_description: "Rust test.".into(),
            critical_rules: vec![
                rule("Observed-tier note", RuleSource::CodeRabbit, RuleConfidence::Observed),
                rule("Hard-tier invariant", RuleSource::Immune, RuleConfidence::Hard),
                rule("Strong-tier convention", RuleSource::CodeRabbit, RuleConfidence::Strong),
            ],
            ..Default::default()
        };
        let rendered = render_root_claude_md(&snapshot, 200);
        let hard_idx = rendered.find("🛡 Hard rules").expect("hard heading");
        let strong_idx = rendered.find("⚠️ Strong conventions").expect("strong heading");
        let observed_idx = rendered.find("📊 Observed patterns").expect("observed heading");
        assert!(hard_idx < strong_idx, "Hard must render before Strong");
        assert!(strong_idx < observed_idx, "Strong must render before Observed");
        assert!(rendered.contains("Hard-tier invariant"));
        assert!(rendered.contains("Strong-tier convention"));
        assert!(rendered.contains("Observed-tier note"));
    }

    #[test]
    fn confidence_tier_rendering_omits_empty_buckets() {
        // A project with only Hard rules should get only the Hard
        // heading — not two empty placeholder sub-headings underneath.
        let snapshot = ProjectSnapshot {
            project_name: "Demo".into(),
            role_description: "Rust test.".into(),
            critical_rules: vec![rule(
                "Only hard rule",
                RuleSource::Immune,
                RuleConfidence::Hard,
            )],
            ..Default::default()
        };
        let rendered = render_root_claude_md(&snapshot, 200);
        assert!(rendered.contains("🛡 Hard rules"));
        assert!(!rendered.contains("⚠️ Strong"));
        assert!(!rendered.contains("📊 Observed"));
    }

    #[test]
    fn confidence_from_source_tiers_immune_as_hard() {
        // Immune rules always tier as Hard regardless of stats: they
        // come from real production reverts.
        assert_eq!(
            confidence_from_source(&RuleSource::Immune, None, None),
            RuleConfidence::Hard
        );
    }

    #[test]
    fn confidence_from_source_tiers_coderabbit_by_stats() {
        // Very strong empirical signal -> Hard.
        assert_eq!(
            confidence_from_source(&RuleSource::CodeRabbit, Some(0.95), Some(15)),
            RuleConfidence::Hard
        );
        // Solid but not Hard -> Strong.
        assert_eq!(
            confidence_from_source(&RuleSource::CodeRabbit, Some(0.85), Some(6)),
            RuleConfidence::Strong
        );
        // Weak signal -> Observed.
        assert_eq!(
            confidence_from_source(&RuleSource::CodeRabbit, Some(0.50), Some(2)),
            RuleConfidence::Observed
        );
    }

    #[test]
    fn extract_critical_rules_skips_engram_generated_bullets() {
        // Regression guard for the bloat ChatGPT flagged: on a rerun,
        // the engram-generated bullets in the existing CLAUDE.md must
        // NOT be re-ingested as "existing user rules" and spliced back
        // into the next run's output. A single rule with the
        // tell-tale `cr_…` id stays out; a handwritten bullet survives.
        let md = "\
<critical_rules>
- Always null-guard the result of GetAll(). (cr_abc12345)
- 🛡 [immune] Never fetch-then-delete. (immune_deadbeef)
- Custom team convention written by a human.
- 🐰 [CodeRabbit] Null-guard rule aggregated — CodeRabbit pattern, 5 PRs, 80% fix rate
</critical_rules>
";
        let rules = extract_critical_rules_from_existing(md);
        let texts: Vec<&str> = rules.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["Custom team convention written by a human."],
            "only the handwritten bullet should survive re-ingestion; got {:?}",
            texts
        );
    }

    fn zone(path: &str, risk: u8, band: &str, downstream: usize, reasons: &[&str]) -> DangerZone {
        DangerZone {
            file_path: path.into(),
            risk_score: risk,
            risk_band: band.into(),
            total_downstream: downstream,
            reasons: reasons.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn minimal_snapshot() -> ProjectSnapshot {
        ProjectSnapshot {
            project_name: "DemoProject".into(),
            role_description: "Rust CLI tool.".into(),
            languages: vec![LanguageShare {
                language: "rust".into(),
                file_count: 40,
                share_percent: 100.0,
            }],
            build_commands: vec!["cargo build --release".into()],
            danger_zones: vec![zone("src/core.rs", 8, "High", 4200, &["hub"])],
            critical_rules: vec![],
            per_language_rules: vec![LanguageRules {
                language: "rust".into(),
                glob: "**/*.rs".into(),
                bullets: vec![
                    "Methods: snake_case (412/412 detected)".into(),
                    "Errors: anyhow::Result — no unwrap() in library code".into(),
                ],
                sample_files: vec!["src/lib.rs".into()],
            }],
            ..Default::default()
        }
    }

    // ── language_to_globs ──

    #[test]
    fn language_to_globs_known_languages() {
        assert_eq!(language_to_globs("vb"), "**/*.vb");
        assert_eq!(language_to_globs("rust"), "**/*.rs");
        assert_eq!(language_to_globs("typescript"), "**/*.ts,**/*.tsx");
        assert_eq!(language_to_globs("C#"), "**/*.cs");
    }

    #[test]
    fn language_to_globs_unknown_uses_language_as_extension() {
        assert_eq!(language_to_globs("zig"), "**/*.zig");
        assert_eq!(language_to_globs("elixir"), "**/*.elixir");
    }

    #[test]
    fn language_slug_sanitises_punctuation() {
        assert_eq!(language_slug("VB.NET"), "vbnet");
        assert_eq!(language_slug("C++"), "c");
        assert_eq!(language_slug("c_sharp"), "csharp");
        assert_eq!(language_slug(""), "unknown");
    }

    // ── Root CLAUDE.md rendering ──

    #[test]
    fn root_rendering_drops_empty_sections() {
        let snap = ProjectSnapshot {
            project_name: "Tiny".into(),
            role_description: "Python CLI".into(),
            ..Default::default()
        };
        let md = render_root_claude_md(&snap, 60);
        assert!(md.contains("# Tiny"));
        assert!(md.contains("<role>"));
        assert!(md.contains("<engram>"));
        // No data for these — must NOT render.
        assert!(!md.contains("<critical_rules>"));
        assert!(!md.contains("<build>"));
        assert!(!md.contains("<danger_zones>"));
        assert!(!md.contains("<conventions>"));
    }

    #[test]
    fn root_rendering_under_60_lines_by_default() {
        let snap = minimal_snapshot();
        let md = render_root_claude_md(&snap, 60);
        let lines = md.lines().count();
        assert!(
            lines <= 60,
            "root CLAUDE.md must fit in 60 lines, got {lines}"
        );
    }

    #[test]
    fn root_rendering_uses_xml_tag_enclosures() {
        let snap = ProjectSnapshot {
            project_name: "X".into(),
            role_description: "desc".into(),
            critical_rules: vec![CriticalRule {
                text: "Rule A.".into(),
                evidence: Some("(evidence)".into()),
                source: RuleSource::Immune,
                confidence: Default::default(),
            }],
            danger_zones: vec![zone("a.vb", 7, "High", 100, &[])],
            ..Default::default()
        };
        let md = render_root_claude_md(&snap, 60);
        assert!(md.contains("<role>"));
        assert!(md.contains("</role>"));
        assert!(md.contains("<critical_rules>"));
        assert!(md.contains("</critical_rules>"));
        assert!(md.contains("<danger_zones>"));
        assert!(md.contains("</danger_zones>"));
        // Rule evidence appears inline.
        assert!(md.contains("Rule A. (evidence)"));
    }

    #[test]
    fn root_rendering_trims_to_budget() {
        // Force an overflow by packing 40 critical rules + 40 zones.
        let mut snap = minimal_snapshot();
        snap.critical_rules = (0..40)
            .map(|i| CriticalRule {
                text: format!("Rule #{i} is important."),
                evidence: None,
                source: RuleSource::RepoRule,
                confidence: Default::default(),
            })
            .collect();
        snap.danger_zones = (0..40)
            .map(|i| zone(&format!("file{i}.vb"), 5, "Medium", 100 + i, &[]))
            .collect();
        let md = render_root_claude_md(&snap, 50);
        assert!(md.lines().count() <= 50, "got {} lines", md.lines().count());
        // Critical rules survive; engram section is the first to go.
        assert!(md.contains("<critical_rules>"));
    }

    #[test]
    fn root_rendering_danger_zones_capped_at_five() {
        let mut snap = minimal_snapshot();
        snap.danger_zones = (0..10)
            .map(|i| zone(&format!("f{i}.rs"), 8, "High", 1000 + i, &[]))
            .collect();
        let md = render_root_claude_md(&snap, 120);
        assert_eq!(md.matches("- f").count(), 5);
    }

    // ── Rule files ──

    #[test]
    fn rule_files_generated_for_each_language() {
        let snap = ProjectSnapshot {
            project_name: "Multi".into(),
            per_language_rules: vec![
                LanguageRules {
                    language: "rust".into(),
                    glob: "**/*.rs".into(),
                    bullets: vec!["Rust bullet".into()],
                    sample_files: vec!["src/lib.rs".into()],
                },
                LanguageRules {
                    language: "typescript".into(),
                    glob: "**/*.ts,**/*.tsx".into(),
                    bullets: vec!["TS bullet".into()],
                    sample_files: vec!["app.ts".into()],
                },
                LanguageRules {
                    // empty bullets → must NOT produce a file.
                    language: "python".into(),
                    glob: "**/*.py".into(),
                    bullets: vec![],
                    sample_files: vec![],
                },
            ],
            ..Default::default()
        };
        let files = render_rule_files(&snap);
        let names: Vec<&str> = files.iter().map(|f| f.filename.as_str()).collect();
        assert!(names.contains(&"rust-conventions.md"));
        assert!(names.contains(&"typescript-conventions.md"));
        assert!(
            !names.contains(&"python-conventions.md"),
            "empty bullet list must not produce a file"
        );
    }

    #[test]
    fn rule_file_uses_globs_frontmatter_not_paths() {
        let lang = LanguageRules {
            language: "vbnet".into(),
            glob: "**/*.vb".into(),
            bullets: vec!["Methods: PascalCase".into()],
            sample_files: vec!["sharedfunc.vb".into()],
        };
        let md = render_language_rules_with_cr(&lang, &[]);
        assert!(md.contains("globs: \"**/*.vb\""));
        assert!(!md.contains("paths:"), "must use globs: not paths:");
        assert!(md.contains("<instructions>"));
        assert!(md.contains("Methods: PascalCase"));
        assert!(md.contains("sharedfunc.vb"));
    }

    #[test]
    fn splice_with_markers_replaces_only_that_span() {
        let existing = "\
# Project manual

Handwritten guidance the agent needs.

<!-- engram:begin -->
OLD engram block
<!-- engram:end -->

## Section after engram

Another handwritten section.
";
        let new_block = "NEW engram block";
        let out = splice_engram_section(existing, new_block);
        assert!(out.contains("Handwritten guidance"));
        assert!(out.contains("Another handwritten section."));
        assert!(out.contains("NEW engram block"));
        assert!(
            !out.contains("OLD engram block"),
            "old engram content must have been replaced; got:\n{out}"
        );
    }

    #[test]
    fn splice_without_markers_appends_without_touching_existing() {
        let existing = "# Project manual\n\n## Rules\n\n1. Never do X.\n2. Always Y.\n";
        let new_block = "Engram-generated block";
        let out = splice_engram_section(existing, new_block);
        assert!(
            out.starts_with("# Project manual"),
            "existing content must remain at top"
        );
        assert!(out.contains("1. Never do X."));
        assert!(out.contains("Engram-generated block"));
        assert!(out.contains(ENGRAM_BEGIN_MARKER));
        assert!(out.contains(ENGRAM_END_MARKER));
    }

    #[test]
    fn splice_is_idempotent_across_runs() {
        let existing = "# Manual\n\nUser rule 1.\n";
        let block1 = "Run 1 engram content";
        let block2 = "Run 2 engram content";
        let after_run1 = splice_engram_section(existing, block1);
        let after_run2 = splice_engram_section(&after_run1, block2);
        assert!(after_run2.contains("User rule 1."));
        assert!(after_run2.contains("Run 2 engram content"));
        assert!(
            !after_run2.contains("Run 1 engram content"),
            "run 2 must replace the run 1 engram block; got:\n{after_run2}"
        );
        // Only ONE marker pair should remain after rerun.
        assert_eq!(
            after_run2.matches(ENGRAM_BEGIN_MARKER).count(),
            1,
            "exactly one engram block expected"
        );
        assert_eq!(after_run2.matches(ENGRAM_END_MARKER).count(), 1);
    }

    #[test]
    fn optimize_rewrite_replaces_engram_owned_sections() {
        let existing = "\
# My Project

## Critical rules

- stale rule the human copied from an old engram run

## Architecture decisions

Service boundaries at X because of Y domain reason.

## Danger zones

- stale human-written list

## Onboarding

Read docs/internal.md first.
";
        let engram_block = "## Critical rules\n\n- fresh engram rule\n\n## Danger zones\n\n- fresh zone\n";
        let (out, report) = optimize_rewrite(existing, engram_block);

        assert!(
            out.contains("Service boundaries at X"),
            "architecture section must survive (human-only insight); got:\n{out}"
        );
        assert!(
            out.contains("Read docs/internal.md first."),
            "onboarding section must survive"
        );
        assert!(
            out.contains("fresh engram rule"),
            "engram block must be present"
        );
        assert!(
            !out.contains("stale rule the human copied"),
            "engram-owned section must be replaced, not merged; got:\n{out}"
        );

        let replaced_joined = report.replaced_sections.join(" ").to_ascii_lowercase();
        assert!(replaced_joined.contains("critical rules"));
        assert!(replaced_joined.contains("danger zones"));
        let preserved_joined = report.preserved_sections.join(" ");
        assert!(preserved_joined.contains("Architecture decisions"));
        assert!(preserved_joined.contains("Onboarding"));
    }

    #[test]
    fn optimize_rewrite_preserves_numbered_behavioral_manual() {
        // Regression for the real-world bug: a 500-line hand-authored
        // manual with sections like `## 1. Data layer rules` must
        // survive. These aren't on the engram-owned list so every
        // numbered section is preserved verbatim.
        let mut existing = String::from("# OciusX\n\n");
        for i in 1..=5 {
            existing.push_str(&format!(
                "## {i}. Section {i}\n\nDomain rule {i}.\n\n"
            ));
        }
        let engram = "## Critical rules\n\n- engram rule\n";
        let (out, report) = optimize_rewrite(&existing, engram);
        for i in 1..=5 {
            let expected = format!("## {i}. Section {i}");
            assert!(
                out.contains(&expected),
                "numbered human section `{expected}` must survive; got:\n{out}"
            );
            assert!(out.contains(&format!("Domain rule {i}.")));
        }
        assert!(out.contains("engram rule"));
        assert!(
            report.preserved_sections.len() >= 5,
            "5 numbered sections expected; got {:?}",
            report.preserved_sections
        );
    }

    #[test]
    fn critical_rules_section_tags_sources_distinctly() {
        // Root CLAUDE.md must visibly distinguish immune / CodeRabbit
        // / plain-repo / existing rules so the agent can weight them.
        let mut snap = minimal_snapshot();
        snap.critical_rules = vec![
            CriticalRule {
                text: "Never DeleteAllOnSubmit without WHERE".into(),
                evidence: Some("(immune_abc1234)".into()),
                source: RuleSource::Immune,
                confidence: Default::default(),
            },
            CriticalRule {
                text: "Call handelselogg.Create after SubmitChanges".into(),
                evidence: Some("(cr_deadbeef)".into()),
                source: RuleSource::CodeRabbit,
                confidence: Default::default(),
            },
            CriticalRule {
                text: "Use SafeRedirect not Response.Redirect".into(),
                evidence: None,
                source: RuleSource::RepoRule,
                confidence: Default::default(),
            },
        ];
        let md = render_root_claude_md(&snap, 60);
        assert!(
            md.contains("🛡 [immune]") && md.contains("Never DeleteAllOnSubmit"),
            "immune tag expected; got:\n{md}"
        );
        assert!(
            md.contains("🐰 [CodeRabbit]") && md.contains("handelselogg"),
            "CodeRabbit tag expected; got:\n{md}"
        );
        // Plain repo rule shouldn't get a tag — spills the attention
        // budget for the ones that do need one.
        let untagged_line = md
            .lines()
            .find(|l| l.contains("SafeRedirect"))
            .expect("repo rule line");
        assert!(
            !untagged_line.contains('🛡') && !untagged_line.contains('🐰'),
            "plain repo rule must NOT get a provenance emoji: {untagged_line}"
        );
    }

    #[test]
    fn language_rule_file_embeds_coderabbit_top_k() {
        let lang = LanguageRules {
            language: "vbnet".into(),
            glob: "**/*.vb".into(),
            bullets: vec!["Methods: PascalCase".into()],
            sample_files: vec!["sharedfunc.vb".into()],
        };
        let cr_rules: Vec<CodeRabbitRule> = (0..15)
            .map(|i| CodeRabbitRule {
                rule_text: format!("Rule #{i}"),
                fix_rate: 0.8,
                pr_count: 3 + i,
                fix_commit: Some(format!("abc{i:04}")),
                composite_score: 0.5 + (i as f32) * 0.01,
            })
            .collect();
        let md = render_language_rules_with_cr(&lang, &cr_rules);
        assert!(
            md.contains("## CodeRabbit patterns"),
            "section header expected; got:\n{md}"
        );
        // Cap — only 10 should render even though we passed 15.
        let bullet_count = md.matches("\n- **Rule #").count();
        assert_eq!(
            bullet_count, 10,
            "top-K cap must limit rendered CR bullets; got {bullet_count}"
        );
        // The highest-composite ones survive (14 has composite 0.64,
        // 0 has 0.50). Rule #14 must appear; Rule #0 must not.
        assert!(md.contains("Rule #14"), "highest-score rule missing: {md}");
        assert!(
            !md.contains("Rule #0 "),
            "lowest-score rule must have been trimmed: {md}"
        );
    }

    #[test]
    fn language_rule_file_omits_coderabbit_section_when_empty() {
        let lang = LanguageRules {
            language: "python".into(),
            glob: "**/*.py".into(),
            bullets: vec!["Use snake_case".into()],
            sample_files: Vec::new(),
        };
        let md = render_language_rules_with_cr(&lang, &[]);
        assert!(
            !md.contains("CodeRabbit patterns"),
            "section must be omitted when no CR rules are present: {md}"
        );
    }

    #[test]
    fn coderabbit_only_language_still_gets_a_rule_file() {
        // A project that has CodeRabbit patterns for C# but no
        // deterministic style bullets for that language should still
        // get a csharp-conventions.md file.
        let mut snap = minimal_snapshot();
        snap.per_language_rules = Vec::new(); // no deterministic rules
        snap.coderabbit_rules_by_language.insert(
            "csharp".into(),
            vec![CodeRabbitRule {
                rule_text: "await ConfigureAwait(false) on library calls".into(),
                fix_rate: 1.0,
                pr_count: 4,
                fix_commit: Some("fab1234".into()),
                composite_score: 0.9,
            }],
        );
        let files = render_rule_files(&snap);
        let cs_file = files
            .iter()
            .find(|f| f.filename == "csharp-conventions.md")
            .expect("CR-only language must produce a rule file");
        assert!(cs_file.content.contains("CodeRabbit patterns"));
        assert!(cs_file.content.contains("ConfigureAwait"));
    }

    #[test]
    fn danger_zones_file_cites_scores_and_bands() {
        let zones = vec![
            zone("a.vb", 8, "High", 2453, &["state+events"]),
            zone("b.vb", 5, "Medium", 1960, &["immune-flagged"]),
        ];
        let md = render_danger_zones(&zones);
        assert!(md.contains("8/10 High"));
        assert!(md.contains("state+events"));
        assert!(md.contains("immune-flagged"));
        assert!(md.contains("`immune_check`"));
    }

    #[test]
    fn state_and_data_file_omits_missing_sections() {
        let md = render_state_and_data(
            Some(&StateSummary {
                total_state_keys: 10,
                session_keys: 6,
                viewstate_keys: 3,
                application_keys: 1,
                cross_page_chains: 2,
                top_keys: vec![("CartID".into(), 15), ("UserId".into(), 10)],
            }),
            None, // no DB summary
            None, // no auth summary
        );
        assert!(md.contains("Session"));
        assert!(md.contains("CartID"));
        // Sections with no data must not render.
        assert!(!md.contains("## Database tables"));
        assert!(!md.contains("## Auth"));
    }

    // ── Existing CLAUDE.md merge ──

    #[test]
    fn extract_critical_rules_from_xml_block() {
        let md =
            "# Proj\n\n<critical_rules>\n- Rule A.\n- Rule B with detail.\n</critical_rules>\n";
        let rules = extract_critical_rules_from_existing(md);
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].text, "Rule A.");
        assert_eq!(rules[1].text, "Rule B with detail.");
        assert!(matches!(rules[0].source, RuleSource::Existing));
    }

    #[test]
    fn extract_critical_rules_fallback_heading_scan() {
        let md = "# Proj\n\n## Critical rules\n- Fallback rule.\n\n## Other stuff\n- not a rule.\n";
        let rules = extract_critical_rules_from_existing(md);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].text, "Fallback rule.");
    }

    #[test]
    fn merge_dedups_and_preserves_human_text_with_engram_evidence() {
        let engram = vec![CriticalRule {
            text: "SafeRedirect must be followed by Return.".into(),
            evidence: Some("(168 callers, blast radius 4/10)".into()),
            source: RuleSource::RepoRule,
            confidence: Default::default(),
        }];
        let existing = vec![CriticalRule {
            text: "SafeRedirect must be followed by Return".into(), // no period
            evidence: None,
            source: RuleSource::Existing,
            confidence: Default::default(),
        }];
        let merged = merge_with_existing(engram, existing);
        assert_eq!(merged.len(), 1, "duplicate rule must be deduped");
        assert_eq!(merged[0].source, RuleSource::Existing);
        assert_eq!(
            merged[0].evidence.as_deref(),
            Some("(168 callers, blast radius 4/10)"),
            "human text survives, engram evidence is attached"
        );
    }

    #[test]
    fn merge_keeps_non_overlapping_engram_rules() {
        let engram = vec![CriticalRule {
            text: "Rule Only From Engram.".into(),
            evidence: None,
            source: RuleSource::Immune,
            confidence: Default::default(),
        }];
        let existing = vec![CriticalRule {
            text: "Human-only rule.".into(),
            evidence: None,
            source: RuleSource::Existing,
            confidence: Default::default(),
        }];
        let merged = merge_with_existing(engram, existing);
        assert_eq!(merged.len(), 2);
        // Human rule comes first.
        assert_eq!(merged[0].text, "Human-only rule.");
    }

    // ── AGENTS.md ──

    #[test]
    fn agents_md_is_generated_without_engram_block() {
        let snap = minimal_snapshot();
        let md = render_agents_md(&snap, 120);
        assert!(md.contains("agent notes"));
        assert!(md.contains("## Language conventions"));
        assert!(
            !md.contains("<engram>"),
            "AGENTS.md must not include the Claude-specific engram block"
        );
    }

    // ── strip_section ──
    //
    // Regression guard for an earlier version that carried a second
    // `format!("{tag}>\n")` fallback — which produced garbage like
    // `<engram>>\n` and was dead code. These tests pin the current
    // contract: `tag` already includes its angle brackets, and the
    // helper is plain string surgery over that.

    #[test]
    fn strip_section_removes_block_and_trailing_blank() {
        let text = "# Header\n\n<engram>\nbody\n</engram>\n\ntail\n";
        let out = strip_section(text, "<engram>");
        assert_eq!(out, "# Header\n\ntail\n");
    }

    #[test]
    fn strip_section_no_op_when_tag_missing() {
        let text = "# Header\n\nnothing to strip\n";
        assert_eq!(strip_section(text, "<engram>"), text);
    }

    #[test]
    fn strip_section_no_op_when_close_tag_missing() {
        let text = "# Header\n\n<engram>\nbody\nno closing tag\n";
        assert_eq!(strip_section(text, "<engram>"), text);
    }

    #[test]
    fn enforce_root_budget_drops_lowest_priority_first() {
        let snap = ProjectSnapshot {
            project_name: "P".into(),
            role_description: "desc".into(),
            danger_zones: vec![zone("a.rs", 8, "High", 100, &[])],
            build_commands: vec!["cargo build".into()],
            critical_rules: vec![CriticalRule {
                text: "critical".into(),
                evidence: None,
                source: RuleSource::RepoRule,
                confidence: Default::default(),
            }],
            per_language_rules: vec![LanguageRules {
                language: "rust".into(),
                glob: "**/*.rs".into(),
                bullets: vec!["x".into()],
                sample_files: vec![],
            }],
            ..Default::default()
        };
        let full = render_root_claude_md(&snap, 300);
        let full_lines = full.lines().count();
        assert!(
            full_lines > 20,
            "expected a chunky render, got {full_lines}"
        );

        // Squeeze hard — `<engram>` drops first, `<critical_rules>` never does.
        let tight = render_root_claude_md(&snap, full_lines.saturating_sub(3));
        assert!(
            !tight.contains("<engram>"),
            "over-budget render must drop <engram> first, got:\n{tight}"
        );
        assert!(tight.contains("<critical_rules>"));
    }
}
