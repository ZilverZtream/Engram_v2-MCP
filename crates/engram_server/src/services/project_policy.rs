//! Task-scoped, checked-out repository guidance shared by planning and editing.
//!
//! Applicability metadata selects documents, not individual prose mandates. The
//! complete document (including exceptions) remains the policy evidence. Sources
//! that do not fit are referenced explicitly rather than partially quoted as law.

use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;

const MAX_DISCOVERY_ENTRIES: usize = 2_048;
const MAX_CANDIDATES: usize = 512;
const MAX_SOURCE_BYTES: u64 = 512 * 1_024;
const POLICY_INSTRUCTION: &str = "Checked-out repository guidance is policy evidence, not proof of implementation compliance. Read each complete source, including exclusions and examples, and resolve conflicts according to its stated authority. Scope matching selects documents only; Engram does not semantically enforce their prose. Omitted or unresolved documents require reading the referenced source before claiming policy coverage.";

#[derive(Debug, Clone, Copy)]
pub struct PolicyBudget {
    pub max_sources: usize,
    pub max_characters_per_source: usize,
    pub max_characters_total: usize,
}

impl Default for PolicyBudget {
    fn default() -> Self {
        Self {
            max_sources: 8,
            max_characters_per_source: 10_000,
            max_characters_total: 24_000,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PolicySource {
    pub path: String,
    pub blake3: String,
    pub characters_total: usize,
    pub characters_shown: usize,
    pub truncated: bool,
    pub applicability: Vec<String>,
    pub content: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PolicyOmission {
    pub path: String,
    pub reason: String,
    pub blake3: Option<String>,
    pub refetch: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PolicyContext {
    pub status: String,
    pub coverage: String,
    pub sources: Vec<PolicySource>,
    pub sources_discovered: usize,
    pub sources_applicable: usize,
    pub sources_shown: usize,
    pub source_cap: usize,
    pub character_cap_per_source: usize,
    pub character_cap_total: usize,
    pub omissions: Vec<PolicyOmission>,
    pub notes: Vec<String>,
    pub instruction: String,
}

impl PolicyContext {
    fn empty(budget: PolicyBudget) -> Self {
        Self {
            status: "absent".into(),
            coverage: "complete".into(),
            sources: vec![],
            sources_discovered: 0,
            sources_applicable: 0,
            sources_shown: 0,
            source_cap: budget.max_sources,
            character_cap_per_source: budget.max_characters_per_source,
            character_cap_total: budget.max_characters_total,
            omissions: vec![],
            notes: vec![],
            instruction: POLICY_INSTRUCTION.into(),
        }
    }

    /// A cutoff request must not present today's guidance as historical policy.
    pub fn historical_unavailable() -> Self {
        let mut context = Self::empty(PolicyBudget::default());
        context.status = "historical_unavailable".into();
        context.coverage = "not_evaluated".into();
        context.notes.push("Live repository policy was withheld for this historical request. Read instruction files at the requested revision; current files are not evidence of policy at that time.".into());
        context
    }

    fn omit(&mut self, path: &str, reason: impl Into<String>, hash: Option<String>) {
        self.omissions.push(PolicyOmission {
            path: path.into(), reason: reason.into(), blake3: hash,
            refetch: format!("Read the complete project-relative file `{path}` from the checkout; verify its BLAKE3 when supplied. No excerpt establishes its full qualifications."),
        });
    }

    pub fn render_markdown(&self) -> String {
        use std::fmt::Write;
        let mut text = format!(
            "\n## Repository policy sources\n\nStatus: {}; coverage: {}. Applicable: {}; shown: {}.\n\n{}\n",
            self.status,
            self.coverage,
            self.sources_applicable,
            self.sources_shown,
            self.instruction
        );
        for source in &self.sources {
            let _ = writeln!(
                text,
                "\n### `{}`\n\nBLAKE3: `{}`. Applies because: {}.\n\n{}",
                source.path,
                source.blake3,
                source.applicability.join("; "),
                source.content
            );
        }
        for omitted in &self.omissions {
            let _ = writeln!(
                text,
                "\n- Policy source omitted/unresolved: `{}`: {}. {}{}",
                omitted.path,
                omitted.reason,
                omitted.refetch,
                omitted
                    .blake3
                    .as_ref()
                    .map(|hash| format!(" BLAKE3: `{hash}`."))
                    .unwrap_or_default()
            );
        }
        for note in &self.notes {
            let _ = writeln!(text, "\n- Coverage note: {note}");
        }
        text
    }
}

fn normalized_relative(path: &str) -> Option<String> {
    let path = path.replace('\\', "/");
    (!path.is_empty()
        && !path.starts_with('/')
        && !path.contains(':')
        && !path.contains('\0')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != ".."))
    .then_some(path)
}

fn conventional_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "agents.md"
            | "claude.md"
            | "instructions.md"
            | "contributing.md"
            | "copilot-instructions.md"
            | "coding-standards.md"
            | "code-style.md"
            | "code_style.md"
    )
}

fn policy_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    conventional_name(name)
        || name.ends_with(".instructions.md")
        || ((lower.contains(".claude/rules/") || lower.contains(".cursor/rules/"))
            && (name.ends_with(".md") || name.ends_with(".mdc")))
}

fn add_candidate(candidates: &mut BTreeSet<String>, path: String, notes: &mut Vec<String>) {
    if candidates.len() < MAX_CANDIDATES {
        candidates.insert(path);
    } else if !notes
        .iter()
        .any(|note| note.starts_with("Policy candidate discovery"))
    {
        notes.push(format!("Policy candidate discovery capped at {MAX_CANDIDATES}; inspect remaining instruction files directly."));
    }
}

/// Only visit conventional policy directories and target ancestors: no repository
/// source crawl, no symlink traversal, and no dependency/build directories.
fn discover(
    root: &Path,
    targets: &[String],
    indexed: &[String],
    notes: &mut Vec<String>,
) -> BTreeSet<String> {
    let mut candidates = BTreeSet::new();
    let mut ancestors = BTreeSet::from([String::new()]);
    for target in targets {
        let Some(target) = normalized_relative(target) else {
            notes.push(format!("Invalid policy target path withheld: {target:?}"));
            continue;
        };
        let parts = target.split('/').collect::<Vec<_>>();
        for end in 1..parts.len() {
            ancestors.insert(parts[..end].join("/"));
        }
    }
    let mut visited = 0usize;
    for ancestor in ancestors {
        if visited > MAX_DISCOVERY_ENTRIES {
            break;
        }
        let dir = if ancestor.is_empty() {
            root.to_path_buf()
        } else {
            match engram_core::safe_join(root, &ancestor) {
                Ok(path) => path,
                Err(error) => {
                    notes.push(format!("Policy ancestor {ancestor}: {error}"));
                    continue;
                }
            }
        };
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                notes.push(format!(
                    "Policy ancestor {} could not be inspected: {error}",
                    dir.display()
                ));
                continue;
            }
        };
        for entry in entries {
            visited += 1;
            if visited > MAX_DISCOVERY_ENTRIES {
                break;
            }
            match entry {
                Ok(entry) => {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if conventional_name(&name) {
                        let path = if ancestor.is_empty() {
                            name
                        } else {
                            format!("{ancestor}/{name}")
                        };
                        add_candidate(&mut candidates, path, notes);
                    }
                }
                Err(error) => notes.push(format!("Policy directory entry unavailable: {error}")),
            }
        }
    }
    if visited > MAX_DISCOVERY_ENTRIES {
        notes.push(format!("Policy ancestor discovery capped at {MAX_DISCOVERY_ENTRIES} entries; inspect remaining ancestor guidance directly."));
    }
    // A busy source directory must not prevent discovery of newly authored
    // topic guidance. Conventional policy directories have an independent
    // entry budget and are opened directly, without scanning .github siblings.
    visited = 0;
    if engram_core::safe_join(root, ".github/copilot-instructions.md")
        .is_ok_and(|path| path.is_file())
    {
        add_candidate(
            &mut candidates,
            ".github/copilot-instructions.md".into(),
            notes,
        );
    }
    let mut directories = vec![
        ".github/instructions".to_string(),
        ".claude/rules".into(),
        ".cursor/rules".into(),
    ];
    while let Some(relative) = directories.pop() {
        if visited > MAX_DISCOVERY_ENTRIES {
            break;
        }
        let path = match engram_core::safe_join(root, &relative) {
            Ok(path) => path,
            Err(error) => {
                notes.push(format!("Policy directory {relative} withheld: {error}"));
                continue;
            }
        };
        let entries = match std::fs::read_dir(path) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                notes.push(format!("Policy directory {relative} unavailable: {error}"));
                continue;
            }
        };
        let mut children = Vec::new();
        for entry in entries {
            visited += 1;
            if visited > MAX_DISCOVERY_ENTRIES {
                break;
            }
            match entry {
                Ok(entry) => children.push(entry),
                Err(error) => notes.push(format!("Policy directory entry unavailable: {error}")),
            }
        }
        children.sort_by_key(|entry| entry.file_name());
        for entry in children {
            let name = entry.file_name().to_string_lossy().into_owned();
            let child = format!("{relative}/{name}");
            let kind = match entry.file_type() {
                Ok(kind) => kind,
                Err(error) => {
                    notes.push(format!("Policy entry {child} unavailable: {error}"));
                    continue;
                }
            };
            if kind.is_symlink() {
                notes.push(format!("Policy symlink {child} was not followed."));
                continue;
            }
            if kind.is_dir() {
                if relative != ".github" || name == "instructions" {
                    directories.push(child);
                }
            } else if kind.is_file() && policy_path(&child) {
                add_candidate(&mut candidates, child, notes);
            }
        }
    }
    if visited > MAX_DISCOVERY_ENTRIES {
        notes.push(format!("Policy filesystem discovery capped at {MAX_DISCOVERY_ENTRIES} directory entries; inspect remaining policy directories directly."));
    }
    for path in indexed {
        if let Some(path) = normalized_relative(path).filter(|path| policy_path(path)) {
            add_candidate(&mut candidates, path, notes);
        }
    }
    let mut identities = BTreeSet::new();
    candidates.retain(|path| {
        identities.insert(if cfg!(windows) {
            path.to_ascii_lowercase()
        } else {
            path.clone()
        })
    });
    candidates
}

fn split_patterns(text: &str) -> Result<Vec<String>, String> {
    let mut depth = 0i32;
    let mut start = 0;
    let mut patterns = Vec::new();
    for (offset, character) in text.char_indices() {
        match character {
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth -= 1;
                if depth < 0 {
                    return Err("unbalanced scope pattern".into());
                }
            }
            ',' if depth == 0 => {
                patterns.push(text[start..offset].trim().to_string());
                start = offset + 1;
            }
            _ => {}
        }
    }
    if depth != 0 {
        return Err("unbalanced scope pattern".into());
    }
    patterns.push(text[start..].trim().to_string());
    if patterns.iter().any(|pattern| pattern.is_empty()) || patterns.len() > 128 {
        return Err("scope must contain 1 to 128 nonempty patterns".into());
    }
    Ok(patterns)
}

fn glob_expression(pattern: &str) -> Result<String, String> {
    let mut expression = String::new();
    let mut characters = pattern.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '*' if characters.peek() == Some(&'*') => {
                characters.next();
                if characters.peek() == Some(&'/') { characters.next(); expression.push_str("(?:.*/)?"); }
                else { expression.push_str(".*"); }
            }
            '*' => expression.push_str("[^/]*"),
            '?' => expression.push_str("[^/]"),
            '{' => expression.push_str("(?:"),
            '}' => expression.push(')'),
            ',' => expression.push('|'),
            '[' | ']' => return Err("character-class scope patterns are unsupported; read source applicability directly".into()),
            other => expression.push_str(&regex::escape(&other.to_string())),
        }
    }
    Ok(expression)
}

fn compile_scope(pattern: &str) -> Result<regex::Regex, String> {
    let normalized = pattern.replace('\\', "/");
    let normalized = normalized.strip_prefix("./").unwrap_or(&normalized);
    if normalized.len() > 2_048
        || normalized.starts_with('/')
        || normalized.contains(':')
        || normalized.split('/').any(|part| part == "..")
        || normalized.starts_with('!')
    {
        return Err("invalid or unsupported project-relative scope pattern".into());
    }
    regex::RegexBuilder::new(&format!("^{}$", glob_expression(normalized)?))
        .case_insensitive(cfg!(windows))
        .size_limit(256 * 1_024)
        .build()
        .map_err(|error| format!("invalid scope pattern: {error}"))
}

fn frontmatter(content: &str) -> Result<Option<serde_yaml::Mapping>, String> {
    let mut lines = content.trim_start_matches('\u{feff}').lines();
    if lines.next().map(str::trim) != Some("---") {
        return Ok(None);
    }
    let mut yaml = String::new();
    let mut closed = false;
    for line in lines {
        if matches!(line.trim(), "---" | "...") {
            closed = true;
            break;
        }
        yaml.push_str(line);
        yaml.push('\n');
    }
    if !closed {
        return Err("unclosed YAML frontmatter; applicability unresolved".into());
    }
    serde_yaml::from_str::<serde_yaml::Mapping>(&yaml)
        .map(Some)
        .map_err(|error| format!("invalid YAML frontmatter; applicability unresolved: {error}"))
}

fn in_directory(target: &str, parent: &str) -> bool {
    if cfg!(windows) {
        target
            .to_ascii_lowercase()
            .starts_with(&format!("{}/", parent.to_ascii_lowercase()))
    } else {
        target.starts_with(&format!("{parent}/"))
    }
}

fn applicability(path: &str, content: &str, targets: &[String]) -> Result<Vec<String>, String> {
    let lower = path.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    let parent = path
        .rsplit_once('/')
        .map(|(parent, _)| parent)
        .unwrap_or("");
    let centralized = lower == ".github/copilot-instructions.md"
        || lower.starts_with(".github/instructions/")
        || lower.starts_with(".claude/rules/")
        || lower.starts_with(".cursor/rules/");
    // Frontmatter may narrow directory guidance; it cannot grant that file
    // authority over an unrelated subtree.
    if conventional_name(name)
        && !centralized
        && !parent.is_empty()
        && !targets.iter().any(|target| in_directory(target, parent))
    {
        return Ok(Vec::new());
    }
    let metadata = frontmatter(content)?;
    let mut patterns = Vec::new();
    if let Some(metadata) = metadata.as_ref() {
        for field in ["applyTo", "globs", "paths"] {
            let Some(value) = metadata.get(&serde_yaml::Value::String(field.into())) else {
                continue;
            };
            match value {
                serde_yaml::Value::String(text) => patterns.extend(split_patterns(text)?),
                serde_yaml::Value::Sequence(values) => {
                    for value in values {
                        let Some(text) = value.as_str() else {
                            return Err(format!(
                                "{field} must contain strings; applicability unresolved"
                            ));
                        };
                        patterns.extend(split_patterns(text)?);
                    }
                }
                _ => {
                    return Err(format!(
                        "{field} must be a string or string list; applicability unresolved"
                    ));
                }
            }
            if patterns.is_empty() {
                return Err(format!("empty {field}; applicability unresolved"));
            }
        }
    }
    if !patterns.is_empty() {
        let compiled = patterns
            .iter()
            .map(|pattern| compile_scope(pattern))
            .collect::<Result<Vec<_>, _>>()?;
        if patterns.iter().any(|pattern| pattern == "**") {
            return Ok(vec!["explicit repository guidance scope **".into()]);
        }
        return Ok(targets
            .iter()
            .filter(|target| compiled.iter().any(|pattern| pattern.is_match(target)))
            .map(|target| format!("scope matches `{target}`"))
            .take(12)
            .collect());
    }
    let lower = path.to_ascii_lowercase();
    let parent = path
        .rsplit_once('/')
        .map(|(parent, _)| parent)
        .unwrap_or("");
    if lower == ".github/copilot-instructions.md" {
        return Ok(vec!["shared repository guidance".into()]);
    }
    if lower.starts_with(".github/instructions/") || lower.starts_with(".cursor/rules/") {
        let always = metadata
            .as_ref()
            .and_then(|mapping| mapping.get(&serde_yaml::Value::String("alwaysApply".into())))
            .and_then(serde_yaml::Value::as_bool);
        if always == Some(true) {
            return Ok(vec!["explicit alwaysApply repository guidance".into()]);
        }
        return Err(
            "topic document has no applyTo/globs/paths scope; applicability requires manual review"
                .into(),
        );
    }
    if lower.starts_with(".claude/rules/") {
        return Ok(vec!["unscoped shared repository rule file".into()]);
    }
    if parent.is_empty() {
        return Ok(vec!["root repository guidance".into()]);
    }
    Ok(targets
        .iter()
        .filter(|target| in_directory(target, parent))
        .map(|target| format!("directory guidance for `{target}`"))
        .take(12)
        .collect())
}

pub fn resolve(root: &Path, target_paths: &[String], indexed_paths: &[String]) -> PolicyContext {
    resolve_with_budget(root, target_paths, indexed_paths, PolicyBudget::default())
}

pub fn resolve_with_budget(
    root: &Path,
    target_paths: &[String],
    indexed_paths: &[String],
    budget: PolicyBudget,
) -> PolicyContext {
    let mut context = PolicyContext::empty(budget);
    let targets = target_paths
        .iter()
        .filter_map(|path| normalized_relative(path))
        .collect::<Vec<_>>();
    let candidates = discover(root, target_paths, indexed_paths, &mut context.notes);
    context.sources_discovered = candidates.len();
    let mut applicable_sources = BTreeMap::new();
    for relative in candidates {
        let full = match engram_core::safe_join(root, &relative) {
            Ok(full) => full,
            Err(error) => {
                context.omit(&relative, format!("source path withheld: {error}"), None);
                continue;
            }
        };
        if !std::fs::symlink_metadata(&full).is_ok_and(|metadata| metadata.is_file()) {
            context.omit(&relative, "source is not an available regular file", None);
            continue;
        }
        let file = match engram_core::safe_open_read(root, &relative) {
            Ok(file) => file,
            Err(error) => {
                context.omit(&relative, format!("source unavailable: {error}"), None);
                continue;
            }
        };
        if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
            context.omit(&relative, "source is not a readable regular file", None);
            continue;
        }
        let mut bytes = Vec::new();
        if let Err(error) = file.take(MAX_SOURCE_BYTES + 1).read_to_end(&mut bytes) {
            context.omit(&relative, format!("source read failed: {error}"), None);
            continue;
        }
        if bytes.len() as u64 > MAX_SOURCE_BYTES {
            context.omit(
                &relative,
                format!(
                    "source exceeds {MAX_SOURCE_BYTES} byte read limit; applicability unresolved"
                ),
                None,
            );
            continue;
        }
        let hash = blake3::hash(&bytes).to_hex().to_string();
        let content = match String::from_utf8(bytes) {
            Ok(content) => content,
            Err(_) => {
                context.omit(
                    &relative,
                    "source is not valid UTF-8; applicability unresolved",
                    Some(hash),
                );
                continue;
            }
        };
        let reasons = match applicability(&relative, &content, &targets) {
            Ok(reasons) => reasons,
            Err(reason) => {
                context.omit(&relative, reason, Some(hash));
                continue;
            }
        };
        if reasons.is_empty() {
            continue;
        }
        let shared = reasons
            .iter()
            .any(|reason| reason.contains("repository guidance"));
        let characters_total = content.chars().count();
        applicable_sources.insert(
            (!shared, relative.clone()),
            PolicySource {
                path: relative,
                blake3: hash,
                characters_total,
                characters_shown: characters_total,
                truncated: false,
                applicability: reasons,
                content,
            },
        );
    }
    context.sources_applicable = applicable_sources.len();
    let mut remaining = budget.max_characters_total;
    for source in applicable_sources.into_values() {
        let reason = if context.sources.len() >= budget.max_sources {
            Some("applicable source count budget exceeded")
        } else if source.characters_total > budget.max_characters_per_source {
            Some(
                "complete source exceeds per-source character budget; no partial policy was substituted",
            )
        } else if source.characters_total > remaining {
            Some(
                "complete source exceeds remaining total character budget; no partial policy was substituted",
            )
        } else {
            None
        };
        if let Some(reason) = reason {
            context.omit(&source.path, reason, Some(source.blake3));
        } else {
            remaining -= source.characters_total;
            context.sources.push(source);
        }
    }
    context.sources_shown = context.sources.len();
    if !context.omissions.is_empty() || !context.notes.is_empty() {
        context.status = "incomplete".into();
        context.coverage = "incomplete".into();
    } else if !context.sources.is_empty() {
        context.status = "present".into();
    }
    context
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_topic_scope_applies_even_before_candidate_files_are_known() {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            ".github/instructions/global.instructions.md",
            "---\napplyTo: '**'\n---\nGlobal rule.",
        );
        let context = resolve(temp.path(), &[], &[]);
        assert_eq!(context.sources.len(), 1);
        assert_eq!(context.coverage, "complete");
    }

    fn write(root: &Path, path: &str, text: &str) {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, text).unwrap();
    }

    #[test]
    fn live_scopes_select_late_topic_and_ancestors_without_indexing() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(
            root,
            "AGENTS.md",
            "Read exclusions in the applicable topic.",
        );
        write(
            root,
            "Site/maps/INSTRUCTIONS.md",
            "Keep coordinates in longitude/latitude order.",
        );
        write(root, "Site/billing/AGENTS.md", "Billing-only policy.");
        for n in 0..20 {
            write(
                root,
                &format!(".github/instructions/a{n:02}.instructions.md"),
                "---\napplyTo: '**/billing/**'\n---\nBilling only.",
            );
        }
        write(
            root,
            ".github/instructions/z-map.instructions.md",
            "---\napplyTo: '**/maps/**,**/io-*'\n---\nNever omit permission checks. Excludes generated reports.",
        );
        let context = resolve(
            root,
            &["Site/maps/map.ts".into()],
            &["Site/billing/AGENTS.md".into()],
        );
        assert_eq!(context.status, "present", "{context:?}");
        assert_eq!(context.sources.len(), 3);
        assert!(
            context
                .sources
                .iter()
                .any(|source| source.path.ends_with("z-map.instructions.md")
                    && source.content.contains("Excludes generated reports."))
        );
        assert!(
            context
                .sources
                .iter()
                .all(|source| !source.path.contains("billing"))
        );
        assert!(
            context.sources.iter().all(|source| source.blake3
                == blake3::hash(source.content.as_bytes()).to_hex().to_string())
        );
    }

    #[test]
    fn complete_sources_or_explicit_hash_bound_recovery_never_cut_exceptions() {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            "AGENTS.md",
            "Always check access. Exception: generated report HTML is excluded.",
        );
        let context = resolve_with_budget(
            temp.path(),
            &["a.rs".into()],
            &[],
            PolicyBudget {
                max_sources: 1,
                max_characters_per_source: 20,
                max_characters_total: 20,
            },
        );
        assert_eq!(context.coverage, "incomplete");
        assert!(context.sources.is_empty());
        assert_eq!(context.omissions.len(), 1);
        assert_eq!(context.omissions[0].path, "AGENTS.md");
        assert!(context.omissions[0].blake3.is_some());
        assert!(
            context
                .render_markdown()
                .contains("Read the complete project-relative file")
        );
    }

    #[test]
    fn malformed_scopes_are_unresolved_and_cannot_become_global() {
        let temp = tempfile::tempdir().unwrap();
        for (name, scope) in [
            ("number", "applyTo: 7"),
            ("empty", "applyTo: ''"),
            ("negative", "applyTo: '!Site/Q/**'"),
            ("yaml", "applyTo: ["),
        ] {
            write(
                temp.path(),
                &format!(".github/instructions/{name}.instructions.md"),
                &format!("---\n{scope}\n---\nNever change anything."),
            );
        }
        let context = resolve(temp.path(), &["Site/maps/a.ts".into()], &[]);
        assert!(context.sources.is_empty());
        assert_eq!(context.omissions.len(), 4);
        assert_eq!(context.coverage, "incomplete");
    }

    #[test]
    fn shared_glob_parser_handles_lists_braces_and_directory_boundaries() {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            ".github/instructions/a.instructions.md",
            "---\napplyTo: ['Site/maps/*.{ts,tsx}', '**/orders/*.vb']\n---\nScoped rule.",
        );
        for path in ["Site/maps/a.ts", "Site/maps/a.tsx", "orders/a.vb"] {
            assert_eq!(
                resolve(temp.path(), &[path.into()], &[]).sources.len(),
                1,
                "{path}"
            );
        }
        for path in [
            "Site/maps/nested/a.ts",
            "Site/maps/a.css",
            "Site/orders/nested/a.vb",
        ] {
            assert!(
                resolve(temp.path(), &[path.into()], &[]).sources.is_empty(),
                "{path}"
            );
        }
    }

    #[test]
    fn rereads_hashes_and_historical_context_does_not_claim_live_policy() {
        let temp = tempfile::tempdir().unwrap();
        write(temp.path(), "AGENTS.md", "First rule.");
        let before = resolve(temp.path(), &["a.rs".into()], &[]);
        write(temp.path(), "AGENTS.md", "Second rule.");
        let after = resolve(temp.path(), &["a.rs".into()], &[]);
        assert_ne!(before.sources[0].blake3, after.sources[0].blake3);
        let historical = PolicyContext::historical_unavailable();
        assert_eq!(historical.status, "historical_unavailable");
        assert_eq!(historical.coverage, "not_evaluated");
        assert!(historical.sources.is_empty());
        assert!(historical.render_markdown().contains("withheld"));
    }

    #[test]
    fn nested_guidance_cannot_expand_its_authority_through_global_frontmatter() {
        let temporary = tempfile::tempdir().unwrap();
        let policy = "Site/admin/AGENTS.md";
        write(
            temporary.path(),
            policy,
            "---\napplyTo: '**'\n---\nAdmin rules.",
        );
        let unrelated = resolve(
            temporary.path(),
            &["Site/public/a.ts".into()],
            &[policy.into()],
        );
        assert!(unrelated.sources.is_empty());
        let related = resolve(
            temporary.path(),
            &["Site/admin/a.ts".into()],
            &[policy.into()],
        );
        assert_eq!(related.sources.len(), 1);
    }

    #[cfg(windows)]
    #[test]
    fn windows_discovery_deduplicates_index_path_casing() {
        let temporary = tempfile::tempdir().unwrap();
        write(temporary.path(), "Site/admin/AGENTS.md", "Admin rules.");
        let context = resolve(
            temporary.path(),
            &["site/ADMIN/a.ts".into()],
            &["Site/admin/AGENTS.md".into()],
        );
        assert_eq!(context.sources.len(), 1);
    }
}
