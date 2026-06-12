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
    /// True when the graph has spatial_call edges - gates the GIS line in
    /// the <engram> tool manual.
    pub has_gis: bool,
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
    out.push_str("This project is indexed by Engram MCP. Workflow for ANY feature or story:\n");
    out.push_str(
        "1. `plan_user_story` - START HERE: expands a one-line story into concepts, \
         touchpoints, exemplars, and a completion checklist\n",
    );
    out.push_str(
        "2. `get_concept_footprint` - every place a concept lives (don't edit 2 of its 17 \
         touchpoints)\n",
    );
    out.push_str("3. `map_guards_and_settings` - permission checks + settings gating your area\n");
    out.push_str(
        "4. `find_similar_changes` - companion artifacts past changes included that yours is \
         missing (admin page, menu entry)\n",
    );
    out.push_str(
        "5. `check_edit_safety` per method + `compute_blast_radius` before large refactors; \
         `immune_check` on danger-zone files\n",
    );
    out.push_str("6. `pre_commit_review` before every commit\n");
    if snapshot.has_gis {
        out.push_str(
            "Map work: `get_gis_inventory` first - map API usage, configs, layer inventory.\n",
        );
    }
    out.push_str(
        "Also: `trace_ui_event` for postbacks/handlers; `analyze_file_coding_style` before \
         writing in unfamiliar files.\n",
    );
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
    let mut emitted_langs: std::collections::HashSet<String> = std::collections::HashSet::new();
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
        // `render_language_rules_with_cr` returns `None` when the file
        // would be too thin or the language is a placeholder (`unknown`,
        // `other`). Skip those instead of writing a hollow file that
        // erodes trust in the collection.
        if let Some(content) = render_language_rules_with_cr(lang, &ranked) {
            files.push(RuleFile {
                filename: format!("{}-conventions.md", language_slug(&lang.language)),
                content,
            });
            emitted_langs.insert(lang.language.clone());
        }
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
        if let Some(content) = render_language_rules_with_cr(&synthetic_lang, &ranked) {
            files.push(RuleFile {
                filename: format!("{}-conventions.md", language_slug(language)),
                content,
            });
        }
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

/// Bucket a raw language-style bullet into the `Applies to / Mandatory
/// / Strong / Observed` template used by the rewritten convention
/// files. Classification is keyword-driven so it works for any language
/// regardless of what the detector emitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BulletTier {
    /// Structural invariant — transpiled-output markers, `"use strict"`,
    /// required header / footer. Rendered under `## Mandatory`.
    Mandatory,
    /// Strong preference — sample-consistent convention that survives
    /// across files (e.g., "all three sampled files use `var`").
    Strong,
    /// Observed in one or a few sampled files — scope explicitly to
    /// avoid over-generalisation.
    Observed,
    /// Security signal — explicit XSS / innerHTML / injection risk.
    /// Rendered under `## Avoid` so it's read as a prohibition rather
    /// than an observation.
    Avoid,
}

fn classify_bullet(text: &str) -> BulletTier {
    let lower = text.to_ascii_lowercase();
    // Security signals → Avoid (clear prohibition tone).
    if lower.contains("security risk")
        || lower.contains("xss")
        || lower.contains("injection")
        || lower.contains("innerhtml")
        || lower.contains("unsafe")
    {
        return BulletTier::Avoid;
    }
    // Structural invariants → Mandatory.
    if lower.contains("transpiled")
        || lower.contains("use strict")
        || lower.contains("generated js")
        || lower.contains("do not hand-edit")
        || lower.contains("preserve the directive")
    {
        return BulletTier::Mandatory;
    }
    // Heuristic for "strong vs observed": if the bullet cites a
    // ratio like `(N/N)` meaning 100% consistent, it's Strong;
    // anything with `(k/N)` where k<N is Observed — those are the
    // ratios the detector emits when sampled files disagreed.
    if let Some((k, n)) = parse_ratio(&lower) {
        if k == n && n >= 2 {
            return BulletTier::Strong;
        }
        if n > 0 {
            return BulletTier::Observed;
        }
    }
    // Bullets that start with "Don't", "Avoid", or "Never" → Avoid.
    if lower.starts_with("do not")
        || lower.starts_with("don't")
        || lower.starts_with("avoid")
        || lower.starts_with("never")
    {
        return BulletTier::Avoid;
    }
    // Default: treat as Observed. The agent weights it accordingly.
    BulletTier::Observed
}

/// Collapse sample-derived bullets that differ only in their `(k/n)`
/// ratio — e.g. `"Variable declarations: let (10/10)"` and `"Variable
/// declarations: let (3/3)"` from two separate sampled files become a
/// single `"Variable declarations: let (13/13)"` entry. Preserves
/// insertion order so the rendered output matches what the upstream
/// detector emitted.
///
/// Bullets without a ratio pass through unchanged; bullets that appear
/// only once are unaffected. This eliminates the "duplicated `let`
/// bullets" class of noise ChatGPT flagged.
fn dedup_sampled_bullets(bullets: &[String]) -> Vec<String> {
    // (key, first_occurrence, summed_num, summed_den, ratio_count)
    //
    // `key` is the bullet text with every `(k/n)` ratio replaced by a
    // sentinel, so two bullets that share the same narrative but have
    // different sample sizes collapse. `first_occurrence` keeps the
    // original wording (including its first ratio, which we overwrite
    // below with the summed totals when merging).
    struct Slot {
        key: String,
        first: String,
        num: u32,
        den: u32,
        hits: u32,
    }
    let mut slots: Vec<Slot> = Vec::with_capacity(bullets.len());
    for b in bullets {
        let text = b.trim();
        if text.is_empty() {
            continue;
        }
        let (key, ratios) = ratio_dedup_key(text);
        let summed_num: u32 = ratios.iter().map(|(n, _)| *n).sum();
        let summed_den: u32 = ratios.iter().map(|(_, d)| *d).sum();
        if let Some(s) = slots.iter_mut().find(|s| s.key == key) {
            s.num = s.num.saturating_add(summed_num);
            s.den = s.den.saturating_add(summed_den);
            s.hits += 1;
        } else {
            slots.push(Slot {
                key,
                first: text.to_string(),
                num: summed_num,
                den: summed_den,
                hits: 1,
            });
        }
    }
    slots
        .into_iter()
        .map(|s| {
            if s.hits <= 1 || s.den == 0 {
                return s.first;
            }
            // Rewrite the FIRST `(k/n)` occurrence in the preserved
            // bullet text with the summed totals. Any subsequent
            // ratios on the same bullet (uncommon) stay as-is.
            replace_first_ratio(&s.first, s.num, s.den)
        })
        .collect()
}

/// Build the dedup key for a sample-derived bullet and also return
/// every `(k/n)` ratio it contained. The key masks ratios to a fixed
/// sentinel so two bullets with the same narrative collapse.
fn ratio_dedup_key(text: &str) -> (String, Vec<(u32, u32)>) {
    let mut key = String::with_capacity(text.len());
    let mut ratios: Vec<(u32, u32)> = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'(' {
            // Try to parse (digits/digits) — if it works, replace with
            // a sentinel; otherwise emit the paren and continue.
            if let Some((num, den, end)) = try_parse_ratio_at(bytes, i) {
                ratios.push((num, den));
                key.push_str("(…)");
                i = end;
                continue;
            }
        }
        key.push(bytes[i] as char);
        i += 1;
    }
    (key, ratios)
}

fn try_parse_ratio_at(bytes: &[u8], start: usize) -> Option<(u32, u32, usize)> {
    debug_assert_eq!(bytes[start], b'(');
    let mut i = start + 1;
    let num_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == num_start || i >= bytes.len() || bytes[i] != b'/' {
        return None;
    }
    let num: u32 = std::str::from_utf8(&bytes[num_start..i])
        .ok()?
        .parse()
        .ok()?;
    i += 1;
    let den_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == den_start || i >= bytes.len() || bytes[i] != b')' {
        return None;
    }
    let den: u32 = std::str::from_utf8(&bytes[den_start..i])
        .ok()?
        .parse()
        .ok()?;
    Some((num, den, i + 1))
}

fn replace_first_ratio(text: &str, num: u32, den: u32) -> String {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'('
            && let Some((_, _, end)) = try_parse_ratio_at(bytes, i)
        {
            let mut out = String::with_capacity(text.len());
            out.push_str(&text[..i]);
            out.push_str(&format!("({num}/{den})"));
            out.push_str(&text[end..]);
            return out;
        }
        i += 1;
    }
    text.to_string()
}

/// Parse a ratio like `(2/3)` or `(17/17)` embedded in a bullet to
/// drive the Mandatory / Strong / Observed split. Returns `(numerator,
/// denominator)` on success.
fn parse_ratio(text: &str) -> Option<(u32, u32)> {
    let open = text.find('(')?;
    let after = &text[open + 1..];
    let close = after.find(')')?;
    let inner = &after[..close];
    let mut parts = inner.split('/');
    let num = parts.next()?.trim().parse::<u32>().ok()?;
    let den = parts.next()?.trim().parse::<u32>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((num, den))
}

/// Minimum number of substantive lines a rule file must carry to be
/// worth writing to disk. Below this, the file is more noise than
/// signal — the agent learns to ignore shallow files and starts
/// discounting better ones.
const MIN_RULE_FILE_LINES: usize = 3;

/// Short snake_case identifier kept in the agent's context is the
/// language name, so "unknown" gets filtered out entirely rather than
/// producing a hollow `unknown-conventions.md` that undermines trust
/// in the convention-file collection.
fn is_placeholder_language(lang: &str) -> bool {
    let lower = lang.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "unknown" | "" | "other" | "misc" | "txt" | "plain"
    )
}

/// Render a single `LanguageRules` bucket as the `.claude/rules/…md`
/// content. Uses the structured `Applies to / Mandatory / Strong /
/// Observed / Avoid / Examples` template introduced to address the
/// ChatGPT review findings that:
///   - sample-derived claims need explicit scoping (not universal law),
///   - hollow files (empty instructions, only CR patterns) erode trust,
///   - contradictory bullets from different sampled files (camelCase
///     in one, PascalCase in another) need aggregation + caveats.
///
/// Returns `None` when the file would be too thin to be useful —
/// caller should skip it entirely.
fn render_language_rules_with_cr(
    lang: &LanguageRules,
    cr_rules: &[CodeRabbitRule],
) -> Option<String> {
    if is_placeholder_language(&lang.language) {
        return None;
    }

    // Classify every bullet into a tier, deduplicating near-
    // duplicates across sampled files (e.g. `let (10/10)` + `let
    // (3/3)` from two different files → one merged `let (13/13)`).
    // The dedup key ignores the `(k/n)` ratio so textually-
    // equivalent observations with different sample sizes collapse.
    let bullets_dedup = dedup_sampled_bullets(&lang.bullets);
    let mut mandatory: Vec<String> = Vec::new();
    let mut strong: Vec<String> = Vec::new();
    let mut observed: Vec<String> = Vec::new();
    let mut avoid: Vec<String> = Vec::new();
    for b in &bullets_dedup {
        let b_trim = b.trim();
        if b_trim.is_empty() {
            continue;
        }
        match classify_bullet(b_trim) {
            BulletTier::Mandatory => mandatory.push(b.clone()),
            BulletTier::Strong => strong.push(b.clone()),
            BulletTier::Observed => observed.push(b.clone()),
            BulletTier::Avoid => avoid.push(b.clone()),
        }
    }

    // CodeRabbit patterns feed two buckets: high-fix-rate patterns
    // become `## Avoid` (clear anti-patterns reviewers catch
    // repeatedly); lower-fix-rate patterns land under `## Observed`
    // as informational references.
    let mut cr_strong_avoid: Vec<&CodeRabbitRule> = Vec::new();
    let mut cr_observed: Vec<&CodeRabbitRule> = Vec::new();
    let mut ranked: Vec<&CodeRabbitRule> = cr_rules.iter().collect();
    ranked.sort_by(|a, b| {
        b.composite_score
            .partial_cmp(&a.composite_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for r in ranked.iter().take(CODERABBIT_PER_LANGUAGE_CAP) {
        if r.fix_rate >= 0.80 && r.pr_count >= 2 {
            cr_strong_avoid.push(r);
        } else {
            cr_observed.push(r);
        }
    }

    // Signal-bearing sections: Mandatory, Strong, Observed (sample-
    // derived), and anything that reached the Avoid bucket (security
    // or high-fix-rate CR patterns). `cr_observed` is explicitly
    // EXCLUDED from the signal count because it's all low-confidence
    // 1-PR review-note noise — a file with only `## Review
    // observations` is a review-note collection, not a convention
    // file. See the ChatGPT review of OciusX's sql/webforms outputs.
    let signal_lines =
        mandatory.len() + strong.len() + observed.len() + avoid.len() + cr_strong_avoid.len();
    if signal_lines < MIN_RULE_FILE_LINES {
        return None;
    }

    let mut out = String::with_capacity(1024);
    let _ = writeln!(out, "---");
    let _ = writeln!(out, "globs: \"{}\"", lang.glob);
    let _ = writeln!(out, "---");
    out.push('\n');
    let _ = writeln!(out, "# {} conventions", language_display(&lang.language));
    out.push('\n');

    // ## Applies to — scopes the whole file. Explicit about the sample
    // size so readers don't over-generalise from a handful of files.
    out.push_str("## Applies to\n\n");
    let _ = writeln!(out, "Files matching `{}`.", lang.glob);
    if !lang.sample_files.is_empty() {
        let _ = writeln!(
            out,
            "Observations below were aggregated from {} sampled file{}: {}.",
            lang.sample_files.len(),
            if lang.sample_files.len() == 1 {
                ""
            } else {
                "s"
            },
            lang.sample_files.join(", "),
        );
    }
    out.push('\n');

    // ## Mandatory — only when there's at least one.
    if !mandatory.is_empty() {
        out.push_str("## Mandatory\n\n");
        out.push_str("Invariants that must hold in every file matching the glob above.\n\n");
        for b in &mandatory {
            let t = b.trim().trim_start_matches(['-', '*', ' ']);
            let _ = writeln!(out, "- {t}");
        }
        out.push('\n');
    }

    // ## Strong preferences — sample-consistent conventions.
    if !strong.is_empty() {
        out.push_str("## Strong preferences\n\n");
        out.push_str(
            "Conventions consistent across every sampled file. Match these when \
             extending or adding code alongside existing modules.\n\n",
        );
        for b in &strong {
            let t = b.trim().trim_start_matches(['-', '*', ' ']);
            let _ = writeln!(out, "- {t}");
        }
        out.push('\n');
    }

    // ## Observed in sampled files — explicit scope.
    if !observed.is_empty() {
        out.push_str("## Observed in sampled files\n\n");
        out.push_str(
            "Patterns detected in some (not all) sampled files. Treat as context \
             for matching local style when extending existing modules — **not** \
             universal law. For brand-new files, fall back to the language's \
             standard convention.\n\n",
        );
        for b in &observed {
            let t = b.trim().trim_start_matches(['-', '*', ' ']);
            let _ = writeln!(out, "- {t}");
        }
        out.push('\n');
    }

    // ## Avoid — prohibitions (both detector-flagged + CR high-fix-rate).
    if !avoid.is_empty() || !cr_strong_avoid.is_empty() {
        out.push_str("## Avoid\n\n");
        out.push_str(
            "Anti-patterns flagged by static analysis and/or reviewers catch \
             repeatedly. Treat these as prohibitions — the team fixes them nearly \
             every time they appear.\n\n",
        );
        for b in &avoid {
            let t = b.trim().trim_start_matches(['-', '*', ' ']);
            let _ = writeln!(out, "- {t}");
        }
        for rule in &cr_strong_avoid {
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
        out.push('\n');
    }

    // ## Review observations — lower-confidence CR patterns kept for
    // context but clearly labelled as advisory.
    if !cr_observed.is_empty() {
        out.push_str("## Review observations\n\n");
        out.push_str(
            "Lower-confidence patterns from past reviews. Useful as hints; not \
             enforced consistently enough to be prohibitions.\n\n",
        );
        for rule in &cr_observed {
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

    Some(out)
}

/// Legacy shim: the original renderer returned a plain `String` and
/// callers used it unconditionally. The new Option-returning variant
/// lets the caller skip hollow files entirely. Kept as a thin wrapper
/// so existing tests that call the old path still work.
#[cfg(test)]
fn render_language_rules_with_cr_string(
    lang: &LanguageRules,
    cr_rules: &[CodeRabbitRule],
) -> String {
    render_language_rules_with_cr(lang, cr_rules).unwrap_or_default()
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
    let mut out = String::with_capacity(1024);
    out.push_str("---\n");
    out.push_str("# State management + database surface — rules, not just inventory.\n");
    out.push_str("---\n\n");
    out.push_str("# State and data\n\n");

    // ── Mandatory + Strong rules derived from the underlying stats ──
    // The old version of this file was a map (counts and top-N lists).
    // The new version turns each signal into an actionable rule the
    // agent should follow before touching shared state, and keeps the
    // raw inventory at the bottom as reference.
    let mut mandatory: Vec<String> = Vec::new();
    let mut strong: Vec<String> = Vec::new();

    if let Some(s) = state {
        if s.cross_page_chains > 0 {
            mandatory.push(format!(
                "**{}** state keys are touched by 2+ pages. Before changing the SHAPE or SEMANTICS of any session / viewstate key, run `trace_state_usage(key)` to see every other reader and writer — silent cross-page breakage is the dominant regression mode on this surface.",
                s.cross_page_chains
            ));
        }
        if s.session_keys > 0 && s.session_keys >= s.viewstate_keys * 2 {
            strong.push(format!(
                "Session is the dominant state surface ({} keys vs {} ViewState). Prefer existing session keys over introducing new ones — the highest-traffic keys listed below are strong reuse candidates.",
                s.session_keys, s.viewstate_keys
            ));
        }
        if !s.top_keys.is_empty() {
            if let Some((top_key, top_ops)) = s.top_keys.first() {
                strong.push(format!(
                    "`{}` is the hottest state key ({} ops). Any change to its shape cascades widely — treat it as a published contract, not an implementation detail.",
                    top_key, top_ops
                ));
            }
        }
    }

    if let Some(d) = db {
        if !d.top_tables.is_empty() {
            if let Some((top_table, refs)) = d.top_tables.first() {
                if *refs >= 10 {
                    mandatory.push(format!(
                        "`{}` is the central database table ({} references). A schema change here touches the largest fan-out in the graph — run `get_table_schema` + `find_symbol_references` on every column you plan to rename or retype.",
                        top_table, refs
                    ));
                }
            }
        }
    }

    if let Some(a) = auth {
        if !a.mode.is_empty() {
            strong.push(format!(
                "Auth mode: **{}**. Every API controller action and ASPX `Page_Load` must run its permission check before `IsPostBack` / before any data access — not after.",
                a.mode
            ));
        }
        if !a.required_roles.is_empty() {
            strong.push(format!(
                "Role names are string literals checked across {} guarded function(s): {}. Use these EXACT strings — a typo'd role name fails open or locks everyone out depending on the guard.",
                a.session_auth_patterns,
                a.required_roles
                    .iter()
                    .map(|r| format!("`{r}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }

    if !mandatory.is_empty() {
        out.push_str("## Mandatory\n\n");
        for m in &mandatory {
            let _ = writeln!(out, "- {m}");
        }
        out.push('\n');
    }
    if !strong.is_empty() {
        out.push_str("## Strong preferences\n\n");
        for s in &strong {
            let _ = writeln!(out, "- {s}");
        }
        out.push('\n');
    }

    // ── Inventory (reference, not rules) ──
    // Kept below the rules so the agent reads the rules first but can
    // still look up the raw numbers when it needs them.
    if let Some(s) = state {
        out.push_str("## State inventory (reference)\n\n");
        let _ = writeln!(
            out,
            "- Total distinct state keys: **{}** ({} Session, {} ViewState, {} Application)",
            s.total_state_keys, s.session_keys, s.viewstate_keys, s.application_keys
        );
        if s.cross_page_chains > 0 {
            let _ = writeln!(
                out,
                "- Cross-page state chains: **{}** (keys touched by 2+ pages)",
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
        out.push_str("## Database inventory (reference)\n\n");
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
        out.push_str("## Auth inventory (reference)\n\n");
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

/// Collapse directed TemporalCoupling edges into unique unordered file
/// pairs, keep the strongest, and strip the `file:` id prefix. Pairs where
/// either side no longer matters (designer files churn with their page by
/// construction) are kept - the agent can judge; ordering is weight desc.
pub fn top_co_change_pairs(
    edges: &[engram_graph::Edge],
    limit: usize,
) -> Vec<(String, String, u32)> {
    use std::collections::HashMap;
    let mut best: HashMap<(String, String), u32> = HashMap::new();
    for e in edges {
        let a = e.source_id.strip_prefix("file:").unwrap_or(&e.source_id);
        let b = e.target_id.strip_prefix("file:").unwrap_or(&e.target_id);
        if a == b {
            continue;
        }
        let key = if a <= b {
            (a.to_string(), b.to_string())
        } else {
            (b.to_string(), a.to_string())
        };
        let w = best.entry(key).or_default();
        *w = (*w).max(e.weight);
    }
    let mut pairs: Vec<(String, String, u32)> =
        best.into_iter().map(|((a, b), w)| (a, b, w)).collect();
    pairs.sort_by(|x, y| y.2.cmp(&x.2).then_with(|| x.0.cmp(&y.0)));
    pairs.truncate(limit);
    pairs
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

    // Case 1: at least one begin marker AND at least one end marker.
    //
    // Replace the span from the FIRST begin marker to the LAST end
    // marker. This deliberately collapses any stale nested pairs that
    // accumulated between them — a previous buggy splicer version (or
    // a hand edit) could leave two engram blocks side-by-side, and
    // the right answer is "one canonical block going forward", not
    // "preserve the stale content between blocks as human-authored".
    //
    // If the first begin appears AFTER the last end, treat it as
    // corrupt and fall through to Case 2 so we at least append a
    // clean block instead of producing garbage.
    if let (Some(first_begin), Some(last_end)) = (
        existing.find(ENGRAM_BEGIN_MARKER),
        existing.rfind(ENGRAM_END_MARKER),
    ) && last_end > first_begin
    {
        let end_after = last_end + ENGRAM_END_MARKER.len();
        let mut out = String::with_capacity(existing.len() + wrapped.len());
        out.push_str(&existing[..first_begin]);
        out.push_str(&wrapped);
        out.push_str(&existing[end_after..]);
        return out;
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
pub fn optimize_rewrite(existing: &str, engram_block: &str) -> (String, OptimizeReport) {
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
                rule(
                    "Observed-tier note",
                    RuleSource::CodeRabbit,
                    RuleConfidence::Observed,
                ),
                rule(
                    "Hard-tier invariant",
                    RuleSource::Immune,
                    RuleConfidence::Hard,
                ),
                rule(
                    "Strong-tier convention",
                    RuleSource::CodeRabbit,
                    RuleConfidence::Strong,
                ),
            ],
            ..Default::default()
        };
        let rendered = render_root_claude_md(&snapshot, 200);
        let hard_idx = rendered.find("🛡 Hard rules").expect("hard heading");
        let strong_idx = rendered
            .find("⚠️ Strong conventions")
            .expect("strong heading");
        let observed_idx = rendered
            .find("📊 Observed patterns")
            .expect("observed heading");
        assert!(hard_idx < strong_idx, "Hard must render before Strong");
        assert!(
            strong_idx < observed_idx,
            "Strong must render before Observed"
        );
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
    fn engram_block_teaches_planning_workflow_and_gates_gis_line() {
        let mut snap = ProjectSnapshot {
            project_name: "Tiny".into(),
            role_description: "VB.NET WebForms app".into(),
            ..Default::default()
        };
        let md = render_root_claude_md(&snap, 80);
        assert!(
            md.contains("plan_user_story"),
            "workflow must start with plan_user_story"
        );
        assert!(md.contains("get_concept_footprint"));
        assert!(md.contains("pre_commit_review"));
        assert!(
            !md.contains("get_gis_inventory"),
            "GIS line must be absent without spatial edges"
        );

        snap.has_gis = true;
        let md_gis = render_root_claude_md(&snap, 80);
        assert!(
            md_gis.contains("get_gis_inventory"),
            "GIS line must appear when the project has spatial edges"
        );
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
        // Languages produce a file when they have at least
        // MIN_RULE_FILE_LINES substantive observations. A single-bullet
        // language is now considered too thin (regression guard covered
        // separately in `thin_rule_file_is_skipped_entirely`). Give
        // each language three bullets to hit the threshold.
        let snap = ProjectSnapshot {
            project_name: "Multi".into(),
            per_language_rules: vec![
                LanguageRules {
                    language: "rust".into(),
                    glob: "**/*.rs".into(),
                    bullets: vec![
                        "Rust bullet one (3/3)".into(),
                        "Rust bullet two (3/3)".into(),
                        "Rust bullet three (3/3)".into(),
                    ],
                    sample_files: vec!["src/lib.rs".into()],
                },
                LanguageRules {
                    language: "typescript".into(),
                    glob: "**/*.ts,**/*.tsx".into(),
                    bullets: vec![
                        "TS bullet one (3/3)".into(),
                        "TS bullet two (3/3)".into(),
                        "TS bullet three (3/3)".into(),
                    ],
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
            // Need at least MIN_RULE_FILE_LINES substantive bullets
            // for the file to be emitted rather than skipped as too
            // thin. Adding three concrete bullets keeps this test
            // focused on "the metadata serialises correctly", not on
            // the thin-file skip behaviour (covered separately).
            bullets: vec![
                "Methods: PascalCase (3/3)".into(),
                "`\"use strict\"` declared at top of file".into(),
                "Variable declarations: var (2/3)".into(),
            ],
            sample_files: vec!["sharedfunc.vb".into()],
        };
        let md = render_language_rules_with_cr_string(&lang, &[]);
        assert!(md.contains("globs: \"**/*.vb\""));
        assert!(!md.contains("paths:"), "must use globs: not paths:");
        assert!(md.contains("## Applies to"));
        assert!(md.contains("sharedfunc.vb"));
    }

    #[test]
    fn thin_rule_file_is_skipped_entirely() {
        // Regression guard: when a language produces fewer than three
        // substantive bullets and has no CR patterns, emit no file.
        // Shallow files teach the agent to discount the whole
        // `.claude/rules/` collection.
        let lang = LanguageRules {
            language: "vbnet".into(),
            glob: "**/*.vb".into(),
            bullets: vec!["Methods: PascalCase (1/1)".into()],
            sample_files: vec!["sharedfunc.vb".into()],
        };
        assert!(
            render_language_rules_with_cr(&lang, &[]).is_none(),
            "a single-bullet rule file must be skipped as too thin"
        );
    }

    #[test]
    fn placeholder_language_produces_no_conventions_file() {
        // Languages labelled "unknown" / "other" / "" MUST NOT yield a
        // convention file — they're a detector fallback, not a real
        // language, and an "unknown-conventions.md" erodes trust in
        // the collection.
        for lang_name in &["unknown", "other", ""] {
            let lang = LanguageRules {
                language: (*lang_name).into(),
                glob: "**/*.foo".into(),
                bullets: vec![
                    "bullet 1".into(),
                    "bullet 2".into(),
                    "bullet 3".into(),
                    "bullet 4".into(),
                ],
                sample_files: vec!["x.foo".into()],
            };
            assert!(
                render_language_rules_with_cr(&lang, &[]).is_none(),
                "placeholder language `{lang_name}` must produce no file"
            );
        }
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
    fn splice_collapses_multiple_engram_blocks_into_one() {
        // Regression guard for the OciusX duplicate-block incident:
        // a buggy prior splicer run left the file with two engram
        // blocks side-by-side. The fixed splicer must collapse ALL
        // engram-managed content (first begin → last end) into one
        // fresh block, leaving human-authored content around it
        // intact.
        let existing = "\
# Project manual

Handwritten intro that must survive.

<!-- engram:begin -->
OLD engram block #1 — should disappear entirely
<!-- engram:end -->


## Project-specific guidance (preserved)

<!-- engram:begin -->
OLD engram block #2 — should disappear entirely
<!-- engram:end -->

# Hand-authored section below

Long-form human content the author wrote by hand.
";
        let new_block = "NEW engram block";
        let out = splice_engram_section(existing, new_block);
        assert!(out.contains("Handwritten intro"));
        assert!(out.contains("Hand-authored section below"));
        assert!(out.contains("Long-form human content"));
        assert!(
            !out.contains("OLD engram block #1"),
            "first stale block must be removed; got:\n{out}"
        );
        assert!(
            !out.contains("OLD engram block #2"),
            "second stale block must be removed; got:\n{out}"
        );
        assert!(
            out.contains("NEW engram block"),
            "fresh engram content must land between the original \
             human sections; got:\n{out}"
        );
        // Exactly one begin marker + one end marker should survive.
        let begins = out.matches(ENGRAM_BEGIN_MARKER).count();
        let ends = out.matches(ENGRAM_END_MARKER).count();
        assert_eq!(begins, 1, "expected exactly one begin marker; got {begins}");
        assert_eq!(ends, 1, "expected exactly one end marker; got {ends}");
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
        let engram_block =
            "## Critical rules\n\n- fresh engram rule\n\n## Danger zones\n\n- fresh zone\n";
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
            existing.push_str(&format!("## {i}. Section {i}\n\nDomain rule {i}.\n\n"));
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
        // CR patterns now split across `## Avoid` (high fix-rate,
        // high PR count — prohibitions) and `## Review observations`
        // (lower confidence — advisory). All 15 rules in this
        // fixture pass the 0.80 / 2-PR threshold, so they land in
        // `## Avoid`; the top-K cap (10) still applies.
        let lang = LanguageRules {
            language: "vbnet".into(),
            glob: "**/*.vb".into(),
            bullets: vec![
                "Methods: PascalCase (3/3)".into(),
                "`\"use strict\"` declared at top of file".into(),
                "var (2/3)".into(),
            ],
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
        let md = render_language_rules_with_cr_string(&lang, &cr_rules);
        assert!(
            md.contains("## Avoid"),
            "Avoid section expected for high-fix-rate CR rules; got:\n{md}"
        );
        let bullet_count = md.matches("\n- **Rule #").count();
        assert_eq!(
            bullet_count, 10,
            "top-K cap must limit rendered CR bullets; got {bullet_count}"
        );
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
            bullets: vec![
                "Use snake_case (3/3)".into(),
                "`\"use strict\"` declared at top of file".into(),
                "Transpiled output: file contains helpers".into(),
            ],
            sample_files: Vec::new(),
        };
        let md = render_language_rules_with_cr_string(&lang, &[]);
        assert!(
            !md.contains("## Review observations"),
            "review-observations section must be omitted without CR rules: {md}"
        );
        assert!(
            !md.contains("## Avoid"),
            "Avoid section must be omitted when there are no high-fix-rate CR rules: {md}"
        );
    }

    #[test]
    fn coderabbit_only_language_still_gets_a_rule_file() {
        // A project that has CodeRabbit patterns for C# but no
        // deterministic style bullets for that language should still
        // get a csharp-conventions.md file — with the high-fix-rate
        // pattern surfaced under `## Avoid` so the agent reads it as
        // a prohibition, not an observation.
        let mut snap = minimal_snapshot();
        snap.per_language_rules = Vec::new(); // no deterministic rules
        snap.coderabbit_rules_by_language.insert(
            "csharp".into(),
            vec![
                CodeRabbitRule {
                    rule_text: "await ConfigureAwait(false) on library calls".into(),
                    fix_rate: 1.0,
                    pr_count: 4,
                    fix_commit: Some("fab1234".into()),
                    composite_score: 0.9,
                },
                CodeRabbitRule {
                    rule_text: "Prefer var for obvious types".into(),
                    fix_rate: 1.0,
                    pr_count: 3,
                    fix_commit: None,
                    composite_score: 0.8,
                },
                CodeRabbitRule {
                    rule_text: "Avoid swallowing exceptions".into(),
                    fix_rate: 1.0,
                    pr_count: 2,
                    fix_commit: None,
                    composite_score: 0.7,
                },
            ],
        );
        let files = render_rule_files(&snap);
        let cs_file = files
            .iter()
            .find(|f| f.filename == "csharp-conventions.md")
            .expect("CR-only language must produce a rule file");
        assert!(cs_file.content.contains("## Avoid"));
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
        // Sections with no data must not render — the new template
        // renames them to `## Database inventory (reference)` etc,
        // so both old and new headings must be absent.
        assert!(!md.contains("Database"));
        assert!(!md.contains("## Auth"));
    }

    #[test]
    fn top_co_change_pairs_dedupes_directions_and_sorts_by_weight() {
        fn edge(a: &str, b: &str, w: u32) -> engram_graph::Edge {
            engram_graph::Edge {
                source_id: format!("file:{a}"),
                target_id: format!("file:{b}"),
                namespace: "history".into(),
                language: "text".into(),
                edge_kind: engram_graph::EdgeKind::TemporalCoupling,
                weight: w,
                generation: 1,
                metadata: None,
                updated_at_ms: 0,
            }
        }
        // Undirected storage writes both directions; both must collapse to
        // one pair carrying the max weight.
        let edges = vec![
            edge("a.aspx", "a.aspx.vb", 41),
            edge("a.aspx.vb", "a.aspx", 41),
            edge("menu.master", "admin/nav.ascx", 17),
            edge("x.vb", "x.vb", 99), // self-pair: dropped
        ];
        let pairs = top_co_change_pairs(&edges, 20);
        assert_eq!(pairs.len(), 2);
        assert_eq!(
            pairs[0],
            ("a.aspx".to_string(), "a.aspx.vb".to_string(), 41)
        );
        assert_eq!(pairs[1].2, 17);
        // Limit applies.
        assert_eq!(top_co_change_pairs(&edges, 1).len(), 1);
    }

    #[test]
    fn auth_roles_render_as_exact_string_rule() {
        let md = render_state_and_data(
            None,
            None,
            Some(&AuthSummary {
                mode: "house guard helpers: checkisuserinrole (66x)".into(),
                required_roles: vec!["Administrator".into(), "Worker".into()],
                session_auth_patterns: 66,
            }),
        );
        assert!(
            md.contains("checkisuserinrole"),
            "mode must render:
{md}"
        );
        assert!(
            md.contains("`Administrator`") && md.contains("`Worker`"),
            "role literals must be listed verbatim:
{md}"
        );
        assert!(
            md.contains("66 guarded function"),
            "guarded count must appear:
{md}"
        );
    }

    #[test]
    fn state_and_data_emits_actionable_rules_not_only_inventory() {
        // Regression guard: the previous renderer was a map of counts
        // and top-N lists with no rules. The new renderer must turn
        // the signals into actionable Mandatory / Strong rules that
        // tell the agent what to DO before touching shared state.
        let md = render_state_and_data(
            Some(&StateSummary {
                total_state_keys: 158,
                session_keys: 137,
                viewstate_keys: 20,
                application_keys: 1,
                cross_page_chains: 154,
                top_keys: vec![("fjAdvancedFilter".into(), 15)],
            }),
            Some(&DbSummary {
                table_count: 192,
                top_tables: vec![("fj_fiberjobb".into(), 27)],
            }),
            Some(&AuthSummary {
                mode: "ASP.NET Membership + OAuth2".into(),
                required_roles: vec!["Admin".into()],
                session_auth_patterns: 5,
            }),
        );
        // Mandatory / Strong headings must appear.
        assert!(
            md.contains("## Mandatory"),
            "expected Mandatory section:\n{md}"
        );
        assert!(
            md.contains("## Strong preferences"),
            "expected Strong section:\n{md}"
        );
        // Cross-page chains rule must mention trace_state_usage.
        assert!(
            md.contains("trace_state_usage"),
            "cross-page-chains rule must point at `trace_state_usage`:\n{md}"
        );
        // The central DB table rule must name the table.
        assert!(
            md.contains("fj_fiberjobb"),
            "central-table rule must cite the table:\n{md}"
        );
        // Hottest state key rule must name the key.
        assert!(
            md.contains("fjAdvancedFilter"),
            "hottest-key rule must cite the key:\n{md}"
        );
        // Inventory is still present but below the rules.
        assert!(
            md.contains("## State inventory (reference)"),
            "raw inventory should be kept as a reference section:\n{md}"
        );
        // The rules should appear BEFORE the inventory so the agent
        // reads them first.
        let mand_idx = md.find("## Mandatory").unwrap();
        let inv_idx = md.find("## State inventory").unwrap();
        assert!(
            mand_idx < inv_idx,
            "Mandatory rules must appear before raw inventory"
        );
    }

    #[test]
    fn dedup_sampled_bullets_merges_same_bullet_with_different_ratios() {
        // Regression guard for the ChatGPT-flagged duplicated `let`
        // bullets in TypeScript: two sampled files each emitted
        // "Variable declarations: let (10/10)" and "(3/3)". The
        // dedup pass must collapse them into one entry with the
        // summed ratio.
        let bullets = vec![
            "Variable declarations: **`let`** (10/10) — prefer const".to_string(),
            "Variable declarations: **`let`** (3/3) — prefer const".to_string(),
        ];
        let out = dedup_sampled_bullets(&bullets);
        assert_eq!(out.len(), 1, "duplicates must collapse; got {out:?}");
        assert!(
            out[0].contains("(13/13)"),
            "ratios must sum; got {:?}",
            out[0]
        );
    }

    #[test]
    fn dedup_sampled_bullets_leaves_distinct_bullets_alone() {
        let bullets = vec![
            "Method naming: camelCase (2/3)".to_string(),
            "Method naming: PascalCase (8/9)".to_string(),
            "Event wiring: Handles clauses (2/2)".to_string(),
        ];
        let out = dedup_sampled_bullets(&bullets);
        assert_eq!(out.len(), 3, "distinct bullets must survive; got {out:?}");
    }

    #[test]
    fn dedup_sampled_bullets_passes_through_no_ratio_bullets() {
        let bullets = vec![
            "Use strict mode at top of file".to_string(),
            "Use strict mode at top of file".to_string(), // exact duplicate
            "Prefer composition over inheritance".to_string(),
        ];
        let out = dedup_sampled_bullets(&bullets);
        assert_eq!(
            out.len(),
            2,
            "exact duplicates (no ratios) must also collapse; got {out:?}"
        );
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
