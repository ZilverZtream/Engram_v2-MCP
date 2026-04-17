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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleSource {
    /// Extracted from a repo rule whose id starts with `immune_` — these
    /// represent files that were flagged by the immune system from a
    /// reverted commit.
    Immune,
    /// Repo rule not prefixed `immune_`. Generic anti-pattern guidance.
    RepoRule,
    /// Copied verbatim from the existing CLAUDE.md the user already
    /// authored. Human rules take priority over engram-derived ones on
    /// conflicts.
    Existing,
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
    if !snapshot.critical_rules.is_empty() {
        out.push_str("<critical_rules>\n");
        for rule in &snapshot.critical_rules {
            let text = rule.text.trim();
            match &rule.evidence {
                Some(ev) if !ev.is_empty() => {
                    let _ = writeln!(out, "- {text} {ev}");
                }
                _ => {
                    let _ = writeln!(out, "- {text}");
                }
            }
        }
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
fn strip_section(text: &str, tag: &str) -> String {
    let open = format!("{tag}\n");
    let open_alt = format!("{tag}>\n"); // handles `<engram>`
    let close_name = tag.trim_start_matches('<').trim_end_matches('>');
    let close = format!("</{close_name}>\n");

    let start = text.find(&open).or_else(|| text.find(&open_alt));
    let Some(start) = start else {
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

    // One conventions file per language that produced bullets.
    for lang in &snapshot.per_language_rules {
        if lang.bullets.is_empty() {
            continue;
        }
        files.push(RuleFile {
            filename: format!("{}-conventions.md", language_slug(&lang.language)),
            content: render_language_rules(lang),
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

fn render_language_rules(lang: &LanguageRules) -> String {
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
                    out.push(CriticalRule {
                        text: rule,
                        evidence: None,
                        source: RuleSource::Existing,
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
            out.push(CriticalRule {
                text: rule,
                evidence: None,
                source: RuleSource::Existing,
            });
        }
    }
    out
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
        let md = render_language_rules(&lang);
        assert!(md.contains("globs: \"**/*.vb\""));
        assert!(!md.contains("paths:"), "must use globs: not paths:");
        assert!(md.contains("<instructions>"));
        assert!(md.contains("Methods: PascalCase"));
        assert!(md.contains("sharedfunc.vb"));
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
        }];
        let existing = vec![CriticalRule {
            text: "SafeRedirect must be followed by Return".into(), // no period
            evidence: None,
            source: RuleSource::Existing,
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
        }];
        let existing = vec![CriticalRule {
            text: "Human-only rule.".into(),
            evidence: None,
            source: RuleSource::Existing,
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
}
