//! Source-linked proposed tests from version-checked inferred requirements.
use super::business_source::SourceAudit;
use engram_index::{HybridQuery, HybridSearchEngine};
use serde::Deserialize;
use std::{collections::{BTreeMap, BTreeSet, HashSet, VecDeque}, path::Path};

const RISK_PACK_MAX_BYTES: u64 = 256 * 1024;
const RISK_PACK_MAX_RULES: usize = 128;
const CANONICAL_SWEEP_MAX_SEEDS: usize = 24;
const CANONICAL_SWEEP_MAX_FILES: usize = 2_000;
const CANONICAL_SWEEP_MAX_FILE_BYTES: u64 = 1024 * 1024;
const CANONICAL_SWEEP_MAX_TOTAL_BYTES: u64 = 16 * 1024 * 1024;
const CANONICAL_SWEEP_MAX_RESULTS: usize = 40;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RiskRuleFile {
    version: u32,
    /// Default provenance date inherited by rules that omit introduced_at.
    #[serde(default)]
    introduced_at: Option<String>,
    /// Human-readable source (PR, board, handbook revision, etc.).
    #[serde(default)]
    provenance: Option<String>,
    #[serde(default)]
    rules: Vec<ConfiguredRiskRule>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfiguredRiskRule {
    id: String,
    title: String,
    guidance: String,
    #[serde(default = "default_risk_severity")]
    severity: String,
    #[serde(default)]
    extensions: Vec<String>,
    #[serde(default)]
    path_any: Vec<String>,
    #[serde(default)]
    all_terms: Vec<String>,
    #[serde(default)]
    any_terms: Vec<String>,
    #[serde(default)]
    none_terms: Vec<String>,
    #[serde(default)]
    introduced_at: Option<String>,
    #[serde(default)]
    provenance: Option<String>,
    #[serde(skip)]
    source: String,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ConfiguredRiskPack {
    rules: Vec<ConfiguredRiskRule>,
}

impl ConfiguredRiskPack {
    pub(crate) fn len(&self) -> usize { self.rules.len() }
}

fn default_risk_severity() -> String { "warning".into() }

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfiguredRiskMatch {
    pub id: String,
    pub title: String,
    pub guidance: String,
    pub severity: String,
    pub source: String,
    pub introduced_at: Option<String>,
    pub provenance: Option<String>,
}

fn clean_rule_value(value: &str, max: usize) -> bool {
    !value.trim().is_empty()
        && value.len() <= max
        && !value.chars().any(|ch| ch.is_control())
}

fn normalize_rule(mut rule: ConfiguredRiskRule, source: &str) -> Result<ConfiguredRiskRule, String> {
    if !clean_rule_value(&rule.id, 64)
        || !rule.id.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Err("rule id must be 1-64 ASCII letters, digits, dot, dash or underscore".into());
    }
    if !clean_rule_value(&rule.title, 160) || !clean_rule_value(&rule.guidance, 1500) {
        return Err(format!("rule {} title/guidance is blank, too long or contains controls", rule.id));
    }
    rule.severity = rule.severity.trim().to_ascii_lowercase();
    if !matches!(rule.severity.as_str(), "critical" | "warning" | "info" | "style") {
        return Err(format!("rule {} severity must be critical, warning, info or style", rule.id));
    }
    let lists = [&rule.extensions, &rule.path_any, &rule.all_terms, &rule.any_terms, &rule.none_terms];
    if lists.iter().any(|values| values.len() > 32)
        || lists.iter().flat_map(|values| values.iter())
            .any(|value| !clean_rule_value(value, 128))
    {
        return Err(format!("rule {} has too many predicates or an invalid predicate", rule.id));
    }
    if lists.iter().all(|values| values.is_empty()) {
        return Err(format!("rule {} has no predicates", rule.id));
    }
    rule.id = rule.id.trim().to_string();
    rule.title = rule.title.trim().to_string();
    rule.guidance = rule.guidance.trim().to_string();
    if let Some(date) = rule.introduced_at.as_deref()
        && !valid_yyyy_mm_dd(date)
    {
        return Err(format!("rule {} introduced_at must be YYYY-MM-DD", rule.id));
    }
    if rule.provenance.as_deref().is_some_and(|value| !clean_rule_value(value, 300)) {
        return Err(format!("rule {} provenance is blank, too long or contains controls", rule.id));
    }
    rule.extensions = rule.extensions.into_iter()
        .map(|value| value.trim().trim_start_matches('.').to_ascii_lowercase()).collect();
    rule.path_any = rule.path_any.into_iter()
        .map(|value| value.trim().replace('\\', "/").to_ascii_lowercase()).collect();
    for values in [&mut rule.all_terms, &mut rule.any_terms, &mut rule.none_terms] {
        for value in values.iter_mut() { *value = value.trim().to_ascii_lowercase(); }
    }
    rule.source = source.to_string();
    Ok(rule)
}

pub(crate) fn valid_yyyy_mm_dd(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-' && bytes[7] == b'-'
        && bytes.iter().enumerate().all(|(index, byte)| {
            index == 4 || index == 7 || byte.is_ascii_digit()
        })
        && value[5..7].parse::<u8>().is_ok_and(|month| (1..=12).contains(&month))
        && value[8..10].parse::<u8>().is_ok_and(|day| (1..=31).contains(&day))
}

fn read_risk_rule_file(path: &Path, boundary: &Path, source: &str) -> Result<Vec<ConfiguredRiskRule>, String> {
    if !path.exists() { return Ok(Vec::new()); }
    let boundary = boundary.canonicalize().map_err(|error| format!("{source} rule-pack boundary unavailable: {error}"))?;
    let canonical = path.canonicalize().map_err(|error| format!("{source} rule pack unavailable: {error}"))?;
    if !canonical.starts_with(&boundary) {
        return Err(format!("{source} rule pack resolves outside its configured boundary"));
    }
    let metadata = std::fs::metadata(&canonical).map_err(|error| format!("{source} rule-pack metadata unavailable: {error}"))?;
    if !metadata.is_file() || metadata.len() > RISK_PACK_MAX_BYTES {
        return Err(format!("{source} rule pack must be a file no larger than {RISK_PACK_MAX_BYTES} bytes"));
    }
    let bytes = std::fs::read(&canonical).map_err(|error| format!("{source} rule pack cannot be read: {error}"))?;
    let text = std::str::from_utf8(&bytes).map_err(|_| format!("{source} rule pack must be UTF-8"))?;
    let parsed: RiskRuleFile = serde_yaml::from_str(text)
        .map_err(|error| format!("{source} rule pack is invalid YAML/schema: {error}"))?;
    if parsed.version != 1 { return Err(format!("{source} rule-pack version must be 1")); }
    if parsed.rules.len() > RISK_PACK_MAX_RULES {
        return Err(format!("{source} rule pack exceeds {RISK_PACK_MAX_RULES} rules"));
    }
    let mut seen = HashSet::new();
    if parsed.introduced_at.as_deref().is_some_and(|date| !valid_yyyy_mm_dd(date)) {
        return Err(format!("{source} rule-pack introduced_at must be YYYY-MM-DD"));
    }
    if parsed.provenance.as_deref().is_some_and(|value| !clean_rule_value(value, 300)) {
        return Err(format!("{source} rule-pack provenance is blank, too long or contains controls"));
    }
    let default_introduced_at = parsed.introduced_at;
    let default_provenance = parsed.provenance;
    parsed.rules.into_iter().map(|mut rule| {
        if rule.introduced_at.is_none() { rule.introduced_at = default_introduced_at.clone(); }
        if rule.provenance.is_none() { rule.provenance = default_provenance.clone(); }
        let rule = normalize_rule(rule, source)?;
        if !seen.insert(rule.id.clone()) { return Err(format!("{source} rule pack repeats id {}", rule.id)); }
        Ok(rule)
    }).collect()
}

/// Load organization and repository rule packs on every matrix call. Project
/// rules override global rules by stable id, so policy tuning needs no daemon
/// rebuild or restart. Both files are size/count bounded and use substring
/// predicates rather than executable expressions.
pub(crate) fn load_configured_risk_pack(
    data_dir: &Path,
    project_root: &Path,
) -> (ConfiguredRiskPack, Vec<String>) {
    load_configured_risk_pack_before(data_dir, project_root, None)
}

pub(crate) fn load_configured_risk_pack_before(
    data_dir: &Path,
    project_root: &Path,
    knowledge_before: Option<&str>,
) -> (ConfiguredRiskPack, Vec<String>) {
    let sources = [
        (data_dir.join("rules/test-risk-rules.yaml"), data_dir, "global"),
        (project_root.join(".engram/test-risk-rules.yaml"), project_root, "project"),
    ];
    let mut merged = BTreeMap::new();
    let mut notes = Vec::new();
    for (path, boundary, source) in sources {
        match read_risk_rule_file(&path, boundary, source) {
            Ok(rules) => for rule in rules {
                if let Some(cutoff) = knowledge_before {
                    match rule.introduced_at.as_deref() {
                        Some(date) if date < cutoff => {}
                        Some(date) => {
                            notes.push(format!("configured rule {} excluded: introduced_at {date} is not before historical cutoff {cutoff}", rule.id));
                            continue;
                        }
                        None => {
                            notes.push(format!("configured rule {} excluded: no introduced_at provenance for historical cutoff {cutoff}", rule.id));
                            continue;
                        }
                    }
                }
                merged.insert(rule.id.clone(), rule);
            },
            Err(error) => notes.push(error),
        }
    }
    (ConfiguredRiskPack { rules: merged.into_values().collect() }, notes)
}

pub(crate) fn configured_risk_matches(
    pack: &ConfiguredRiskPack,
    file: &str,
    source: &str,
) -> Vec<ConfiguredRiskMatch> {
    let lower_file = file.replace('\\', "/").to_ascii_lowercase();
    let extension = Path::new(file).extension().and_then(|value| value.to_str())
        .unwrap_or_default().to_ascii_lowercase();
    let lower_source = source.to_ascii_lowercase();
    pack.rules.iter().filter(|rule| {
        (rule.extensions.is_empty() || rule.extensions.iter().any(|value| value == &extension))
            && (rule.path_any.is_empty() || rule.path_any.iter().any(|value| lower_file.contains(value)))
            && rule.all_terms.iter().all(|value| lower_source.contains(value))
            && (rule.any_terms.is_empty() || rule.any_terms.iter().any(|value| lower_source.contains(value)))
            && rule.none_terms.iter().all(|value| !lower_source.contains(value))
    }).map(|rule| ConfiguredRiskMatch {
        id: rule.id.clone(),
        title: rule.title.clone(),
        guidance: rule.guidance.clone(),
        severity: rule.severity.clone(),
        source: rule.source.clone(),
        introduced_at: rule.introduced_at.clone(),
        provenance: rule.provenance.clone(),
    }).collect()
}

pub(crate) fn configured_risk_axes(
    pack: &ConfiguredRiskPack,
    file: &str,
    source: &str,
) -> Vec<(String, String)> {
    configured_risk_matches(pack, file, source).into_iter().map(|rule| {
        let provenance = match (rule.introduced_at.as_deref(), rule.provenance.as_deref()) {
            (Some(date), Some(origin)) => format!("; introduced {date}; provenance {origin}"),
            (Some(date), None) => format!("; introduced {date}"),
            (None, Some(origin)) => format!("; provenance {origin}"),
            (None, None) => "; provenance undated".to_string(),
        };
        (
            rule.title,
            format!("{file}: configured rule `{}` from {} pack{provenance}: {}", rule.id, rule.source, rule.guidance),
        )
    }).collect()
}

fn bounded_names<I>(names: I) -> String
where
    I: IntoIterator<Item = String>,
{
    let mut names = names.into_iter().collect::<Vec<_>>();
    names.sort();
    names.dedup();
    let omitted = names.len().saturating_sub(8);
    names.truncate(8);
    let mut rendered = names.join(", ");
    if omitted > 0 {
        rendered.push_str(&format!(" (+{omitted} more)"));
    }
    rendered
}

fn optional_text_parameters(source: &str, ext: &str) -> Vec<String> {
    use regex::Regex;
    let is_vb = ext.eq_ignore_ascii_case("vb");
    let executable = crate::services::business_outcome_dependencies::executable_lines(source, is_vb);
    let pattern = if is_vb {
        r"(?i)\bOptional\s+([A-Za-z_]\w*)\s+As\s+(?:System\.)?String\b"
    } else {
        // Bounded to a signature-shaped parameter segment so ordinary local
        // initializers are not classified as optional inputs.
        r"(?i)(?:\(|,)\s*(?:string|String)\??\s+([A-Za-z_]\w*)\s*=\s*(?:null|default|[^,)]*)"
    };
    let Ok(regex) = Regex::new(pattern) else { return Vec::new(); };
    regex.captures_iter(&executable)
        .filter_map(|captures| captures.get(1).map(|value| value.as_str().to_string()))
        .take(65)
        .collect()
}

fn bound_presentation_members(source: &str) -> Vec<String> {
    use regex::Regex;
    let patterns = [
        r#"(?i)\bDataField\s*=\s*["']([^"']+)["']"#,
        r#"(?i)\b(?:Eval|Bind)\s*\(\s*["']([^"']+)["']"#,
    ];
    let mut members = Vec::new();
    for pattern in patterns {
        let Ok(regex) = Regex::new(pattern) else { continue; };
        members.extend(regex.captures_iter(source)
            .filter_map(|captures| captures.get(1).map(|value| value.as_str().to_string()))
            .take(65));
    }
    members
}

/// Risk axes derived only from caller-supplied change wording. They remain
/// separate from source-triggered axes so a plan cannot mistake a requested
/// change for evidence that the behavior already exists or assume the wording
/// has passed a human checkpoint.
pub(super) fn intent_risk_axes(intent: Option<&str>) -> Vec<(String, String)> {
    let Some(intent) = intent.map(str::trim).filter(|value| !value.is_empty()) else {
        return Vec::new();
    };
    let lower = intent.to_ascii_lowercase();
    let mut axes = Vec::new();
    if [
        "field id",
        "field_id",
        "field-level",
        "field level",
        "individual field",
        "per-field",
        "field-specific",
        "discriminator",
        "enum",
    ]
    .iter()
    .any(|term| lower.contains(term))
    {
        axes.push((
            "Discriminator compatibility and presenter completeness".into(),
            "supplied change intent introduces or changes a discriminator: test the legacy/default value, every defined and reserved value, an unknown value, persistence/model type, nullability and default parity, label/formatter coverage, and an explicit fallback. Treat numeric mappings and schema constraints as exact assertions only after contract approval; an undefined product mapping remains a blocking decision rather than an implementation guess".into(),
        ));
    }
    let has_literal_contract = (intent.contains('=') || lower.contains("default") || lower.contains("not null"))
        && ["required", "approved", "must", "decision", "invariant"]
            .iter()
            .any(|term| lower.contains(term));
    if has_literal_contract {
        axes.push((
            "Supplied contract invariant fidelity".into(),
            "supplied change intent contains literal mappings or schema constraints: retain each as a candidate machine-checkable assertion until the contract is human-approved, then compare it with the final diff. Missing reserved values, shifted identifiers, changed nullability/defaults, or implementation-selected substitutes fail the gate only when bound to an approved decision".into(),
        ));
    }
    if ["canonical", "prefix", "constant", "token"].iter().any(|term| lower.contains(term)) {
        axes.push((
            "Canonical-token migration completeness".into(),
            "supplied change intent centralizes an identifier/token: reconcile the canonical definition, every producer write, every reader/filter comparison, stored legacy values, and a repository literal sweep; list intentional aliases and deferred residual literals instead of silently widening scope".into(),
        ));
    }
    if ["log", "logging", "audit", "event"].iter().any(|term| lower.contains(term)) {
        axes.push((
            "Event cardinality and failure boundary".into(),
            "supplied change intent affects event/audit output: test no-op, one-field and simultaneous changes, suppressed/bulk paths, caller-owned contexts, persistence-call cardinality, and logger/persistence failures without assuming audit success is atomic with the business write".into(),
        ));
        axes.push((
            "Event payload persistence-to-presentation chain".into(),
            "supplied change intent affects event/audit output: decide which facts are stored and which prose is rendered, then trace legacy and new payloads through persistence/model mappings, formatter or presenter fallbacks, every UI/export/API consumer, and every supported locale. Verify writer culture does not permanently determine reader-facing text".into(),
        ));
    }
    if ["archive", "zip", "kmz", "compressed", "extract"].iter().any(|term| lower.contains(term)) {
        axes.push((
            "Bounded archive processing contract".into(),
            "supplied change intent reads or extracts an archive: require one shared bounded path across every entry point and test entry count, total expanded bytes, per-entry bytes, compression ratio, path depth, duplicate/case-colliding names, traversal/absolute paths, nested archives, allowlisted content types, cancellation, and cleanup after partial failure. Limits and rejection behavior remain contract decisions rather than inferred constants".into(),
        ));
    }
    if ["lazy", "convert", "conversion", "derived", "cache", "retry"]
        .iter()
        .any(|term| lower.contains(term))
    {
        axes.push((
            "Persistent derived-state lifecycle".into(),
            "supplied change intent creates lazy, converted, derived, or cached state: model absent, queued/converting, ready, failed, stale/version-mismatched, deleted, and retryable states explicitly. Test simultaneous first readers, process restart, source replacement, persistent failure suppression, deliberate retry, version invalidation, partial rows/artifacts, and isolation between owners or tenants".into(),
        ));
    }
    if ["icon", "image", "canvas", "object url", "listener", "browser cache"]
        .iter()
        .any(|term| lower.contains(term))
    {
        axes.push((
            "Asynchronous browser-resource lifecycle".into(),
            "supplied change intent creates browser media or callback resources: test repeated attach/detach and route/owner switches, callback completion after disposal, listener cardinality, cache invalidation, object-URL revocation, failed image/canvas/CORS operations, and decoded pixel/dimension limits before allocation. A bounded file byte size alone does not bound decoded memory".into(),
        ));
    }
    if ["dto", "interface", "serialized", "json", "wire", "generated javascript", "generated js"]
        .iter()
        .any(|term| lower.contains(term))
    {
        axes.push((
            "End-to-end wire-contract parity".into(),
            "supplied change intent changes a serialized contract: reconcile server serializer names/types/nullability/defaults and child collections with persisted representation, client DTOs, every reader, and committed generated output. Require the repository's real type-check/build plus a representative serialized fixture; textual name overlap is not proof of wire compatibility".into(),
        ));
    }
    if ["xml", "kml", "xdocument", "xmldocument"].iter().any(|term| lower.contains(term)) {
        axes.push((
            "XML encoding and declaration round trip".into(),
            "supplied change intent reads or rewrites XML-family content: test UTF-8 with and without BOM, UTF-16 LE/BE, non-ASCII text, declarations and namespace attributes. Preserve or deliberately normalize encoding according to the contract, and verify bytes can be reparsed after every mutation/error path".into(),
        ));
    }
    axes
}

#[derive(Debug, Default)]
pub(super) struct CanonicalCallSweep {
    pub attempted: bool,
    pub residuals: Vec<String>,
    pub notes: Vec<String>,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct CanonicalSite {
    member: String,
    file: String,
    line: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum ArgumentSelector {
    Resolved(u32),
    Named(String),
    Ordinal(usize),
}

#[derive(Clone, Copy)]
struct CanonicalSweepCaps {
    files: usize,
    file_bytes: u64,
    total_bytes: u64,
    results: usize,
    candidates: usize,
}

const CANONICAL_SWEEP_CAPS: CanonicalSweepCaps = CanonicalSweepCaps {
    files: CANONICAL_SWEEP_MAX_FILES,
    file_bytes: CANONICAL_SWEEP_MAX_FILE_BYTES,
    total_bytes: CANONICAL_SWEEP_MAX_TOTAL_BYTES,
    results: CANONICAL_SWEEP_MAX_RESULTS,
    candidates: CANONICAL_SWEEP_MAX_SEEDS,
};

type InvocationReport = engram_index::vb_extractor::VbInvocationReport;
type SeedGroups = BTreeMap<(String, ArgumentSelector), BTreeSet<CanonicalSite>>;

/// Find residual VB string arguments at the same lexical callee and argument
/// identity as a canonical member added by the staged or unstaged diff.
/// Roslyn syntax is required; there is no lexical fallback.
pub(super) fn canonical_call_migration_sweep(
    project_root: &Path,
    intent: &str,
    requested_files: &[String],
    diffs: &[crate::services::pre_commit_review_service::DiffFile],
) -> CanonicalCallSweep {
    let mut sweep = canonical_call_migration_sweep_with(
        project_root,
        intent,
        requested_files,
        diffs,
        CANONICAL_SWEEP_CAPS,
        engram_index::vb_extractor::vb_invocations,
    );
    const MAX_NOTES: usize = 64;
    if sweep.notes.len() > MAX_NOTES {
        let omitted = sweep.notes.len() - MAX_NOTES;
        sweep.notes.truncate(MAX_NOTES);
        sweep.notes.push(format!(
            "canonical-call notes truncated; {omitted} additional incomplete-evidence note(s) omitted"
        ));
    }
    sweep
}

fn canonical_call_migration_sweep_with<F>(
    project_root: &Path,
    intent: &str,
    requested_files: &[String],
    diffs: &[crate::services::pre_commit_review_service::DiffFile],
    caps: CanonicalSweepCaps,
    mut analyze: F,
) -> CanonicalCallSweep
where
    F: FnMut(&Path, &str) -> Result<InvocationReport, String>,
{
    let requested = requested_files
        .iter()
        .map(|file| canonical_path_key(file))
        .collect::<HashSet<_>>();
    let mut added_by_file: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
    for diff in diffs {
        let path = diff.path.replace('\\', "/");
        if requested.contains(&canonical_path_key(&path))
            && path.to_ascii_lowercase().ends_with(".vb")
            && !diff.is_binary
        {
            added_by_file
                .entry(path)
                .or_default()
                .extend(diff.added_lines.iter().map(|(line, _)| *line as u32));
        }
    }
    if added_by_file.is_empty() {
        return CanonicalCallSweep::default();
    }

    let root = match project_root.canonicalize() {
        Ok(root) => root,
        Err(error) => {
            return CanonicalCallSweep {
                notes: vec![format!("canonical-call project boundary unavailable: {error}")],
                ..Default::default()
            };
        }
    };
    let compact_intent = compact_member(intent).to_ascii_lowercase();
    let mut notes = Vec::new();
    let mut charged_bytes = 0_u64;
    let mut cached: BTreeMap<String, (String, InvocationReport)> = BTreeMap::new();
    let mut groups: SeedGroups = BTreeMap::new();
    let mut candidate_count = 0_usize;
    let mut candidates_truncated = false;

    for (relative, added_lines) in &added_by_file {
        let Some(source) = read_bounded_vb_source(
            &root,
            relative,
            caps.file_bytes,
            caps.total_bytes.saturating_sub(charged_bytes),
            &mut charged_bytes,
            &mut notes,
        ) else {
            continue;
        };
        let path = root.join(relative);
        let report = match analyze(&path, &source) {
            Ok(report) => report,
            Err(error) => {
                notes.push(format!(
                    "canonical-call Roslyn invocation query failed for {relative}: {error}; no lexical fallback was used"
                ));
                continue;
            }
        };
        note_incomplete_invocation_report(relative, &report, &mut notes);
        for invocation in &report.invocations {
            if invocation.source_index != 0 {
                continue;
            }
            let Some(callee_text) = engram_index::vb_extractor::vb_utf16_span_text(
                &source,
                invocation.callee_span_start,
                invocation.callee_span_length,
            ) else {
                notes.push(format!(
                    "canonical-call Roslyn callee span was invalid for {relative}:{}",
                    invocation.start_line
                ));
                continue;
            };
            let callee = normalize_vb_callee(callee_text);
            for argument in &invocation.arguments {
                if argument.classification != "member_access"
                    || argument.source_index != 0
                    || !line_span_intersects(added_lines, argument.start_line, argument.end_line)
                {
                    continue;
                }
                let Some(expression) = engram_index::vb_extractor::vb_utf16_span_text(
                    &source,
                    argument.span_start,
                    argument.span_length,
                ) else {
                    notes.push(format!(
                        "canonical-call Roslyn argument span was invalid for {relative}:{}",
                        argument.start_line
                    ));
                    continue;
                };
                let member = compact_member(expression);
                if !member_has_canonical_segment(&member)
                    && !compact_intent.contains(&member.to_ascii_lowercase())
                {
                    continue;
                }
                let selector = if let Some(parameter) = argument.parameter_ordinal {
                    ArgumentSelector::Resolved(parameter)
                } else if let Some(name) = argument.name.as_deref() {
                    ArgumentSelector::Named(name.to_ascii_lowercase())
                } else {
                    ArgumentSelector::Ordinal(argument.syntax_ordinal as usize)
                };
                let site = CanonicalSite {
                    member,
                    file: relative.clone(),
                    line: argument.start_line,
                };
                let key = (callee.clone(), selector);
                if groups.get(&key).is_some_and(|sites| sites.contains(&site)) {
                    continue;
                }
                if candidate_count >= caps.candidates {
                    candidates_truncated = true;
                    continue;
                }
                groups.entry(key).or_default().insert(site);
                candidate_count += 1;
            }
        }
        cached.insert(canonical_path_key(relative), (source, report));
    }
    if groups.is_empty() {
        return CanonicalCallSweep {
            notes,
            ..Default::default()
        };
    }

    let mut result = CanonicalCallSweep {
        attempted: true,
        notes,
        ..Default::default()
    };
    if candidates_truncated {
        result.notes.push(format!(
            "canonical-call diff candidates truncated at {}; additional Roslyn candidates are unexamined",
            caps.candidates
        ));
    }

    let mut files = engram_index::ingest::iter_files(&root, &["vb"]);
    for requested_file in requested_files.iter().filter(|file| {
        file.to_ascii_lowercase().ends_with(".vb")
    }) {
        files.push(root.join(requested_file));
    }
    files.sort_by_key(|path| canonical_path_key(&path.to_string_lossy()));
    files.dedup_by(|left, right| {
        canonical_path_key(&left.to_string_lossy())
            == canonical_path_key(&right.to_string_lossy())
    });
    files.retain(|path| {
        path.strip_prefix(&root)
            .ok()
            .map(|relative| {
                !canonical_sweep_path_excluded(&relative.to_string_lossy().replace('\\', "/"))
            })
            .unwrap_or(false)
    });
    files.sort_by_key(|path| {
        let relative = path
            .strip_prefix(&root)
            .ok()
            .map(|path| canonical_path_key(&path.to_string_lossy()))
            .unwrap_or_default();
        (!requested.contains(&relative), relative)
    });
    let required_scan = groups
        .values()
        .flat_map(|sites| sites.iter().map(|site| canonical_path_key(&site.file)))
        .collect::<BTreeSet<_>>();
    let eligible_files = files.len();
    if eligible_files > caps.files {
        files.truncate(caps.files);
        result.notes.push(format!(
            "canonical-call project scan truncated at {} of {eligible_files} eligible VB files",
            caps.files
        ));
    }

    let mut scanned_files = 0_usize;
    let mut failed_files = 0_usize;
    let mut stopped_at_result_cap = false;
    let mut stopped_at_byte_cap = false;
    let mut residual_keys = BTreeSet::new();
    let mut scanned_paths = BTreeSet::new();
    'files: for path in files {
        let Ok(relative_path) = path.strip_prefix(&root) else {
            failed_files += 1;
            continue;
        };
        let relative = relative_path.to_string_lossy().replace('\\', "/");
        let cache_key = canonical_path_key(&relative);
        let (source, report, newly_analyzed) = if let Some((source, report)) = cached.remove(&cache_key) {
            (source, report, false)
        } else {
            if charged_bytes >= caps.total_bytes {
                stopped_at_byte_cap = true;
                break;
            }
            let Some(source) = read_bounded_vb_source(
                &root,
                &relative,
                caps.file_bytes,
                caps.total_bytes - charged_bytes,
                &mut charged_bytes,
                &mut result.notes,
            ) else {
                failed_files += 1;
                continue;
            };
            match analyze(&path, &source) {
                Ok(report) => (source, report, true),
                Err(error) => {
                    failed_files += 1;
                    result.notes.push(format!(
                        "canonical-call Roslyn invocation query failed for {relative}: {error}; no lexical fallback was used"
                    ));
                    continue;
                }
            }
        };
        scanned_files += 1;
        scanned_paths.insert(cache_key);
        if newly_analyzed {
            note_incomplete_invocation_report(&relative, &report, &mut result.notes);
        }
        for invocation in &report.invocations {
            if invocation.source_index != 0 {
                continue;
            }
            let Some(callee_text) = engram_index::vb_extractor::vb_utf16_span_text(
                &source,
                invocation.callee_span_start,
                invocation.callee_span_length,
            ) else {
                result.notes.push(format!(
                    "canonical-call Roslyn callee span was invalid for {relative}:{}",
                    invocation.start_line
                ));
                continue;
            };
            let callee = normalize_vb_callee(callee_text);
            for ((seed_callee, selector), sites) in &groups {
                if &callee != seed_callee {
                    continue;
                }
                let argument = match selector {
                    ArgumentSelector::Resolved(parameter) => invocation
                        .arguments
                        .iter()
                        .find(|argument| argument.parameter_ordinal == Some(*parameter)),
                    ArgumentSelector::Named(name) => invocation.arguments.iter().find(|argument| {
                        argument.name.as_deref().is_some_and(|candidate| candidate.eq_ignore_ascii_case(name))
                    }),
                    ArgumentSelector::Ordinal(ordinal) => invocation.arguments.iter().find(
                        |argument| {
                            argument.name.is_none()
                                && argument
                                    .parameter_ordinal
                                    .unwrap_or(argument.syntax_ordinal)
                                    as usize
                                    == *ordinal
                        },
                    ),
                };
                let Some(argument) = argument else { continue };
                if argument.classification != "string_literal" || argument.source_index != 0 {
                    continue;
                }
                let Some(expression) = engram_index::vb_extractor::vb_utf16_span_text(
                    &source,
                    argument.span_start,
                    argument.span_length,
                ) else {
                    result.notes.push(format!(
                        "canonical-call Roslyn argument span was invalid for {relative}:{}",
                        argument.start_line
                    ));
                    continue;
                };
                let key = (
                    relative.clone(),
                    argument.start_line,
                    seed_callee.clone(),
                    selector.clone(),
                    expression.to_string(),
                );
                if !residual_keys.insert(key) {
                    continue;
                }
                let canonical_sites = sites
                    .iter()
                    .map(|site| format!("`{}` at {}:{}", site.member, site.file, site.line))
                    .collect::<Vec<_>>()
                    .join(", ");
                result.residuals.push(format!(
                    "{relative}:{}: `{}` {} is string literal {}; added-diff candidate(s): {canonical_sites}. Review whether this site is an intentional alias or deferred migration; lexical callee/argument equality does not establish symbol binding or require a change",
                    argument.start_line,
                    callee_text,
                    selector_label(selector),
                    bounded_literal(expression),
                ));
                if result.residuals.len() >= caps.results {
                    stopped_at_result_cap = true;
                    break 'files;
                }
            }
        }
    }
    if stopped_at_byte_cap {
        result.notes.push(format!(
            "canonical-call project scan stopped at the {}-byte total source cap after {scanned_files} file(s); bytes are charged before UTF-8 decoding",
            caps.total_bytes
        ));
    }
    if stopped_at_result_cap {
        result.notes.push(format!(
            "canonical-call residuals truncated at {}; remaining files/invocations are unexamined",
            caps.results
        ));
    }
    if failed_files > 0 {
        result.notes.push(format!(
            "canonical-call scan could not obtain source-bound Roslyn evidence for {failed_files} file(s)"
        ));
    }
    let omitted_requested = required_scan
        .difference(&scanned_paths)
        .cloned()
        .collect::<Vec<_>>();
    if !omitted_requested.is_empty() {
        result.notes.push(format!(
            "canonical-call scan omitted requested seed file(s): {}; same-file residual coverage is incomplete",
            omitted_requested.join(", ")
        ));
    }
    result.summary = format!(
        "Bounded Roslyn VB sweep derived {} canonical callee/argument group(s) containing {candidate_count} added-diff candidate(s), and examined {scanned_files} project file(s), {charged_bytes} source byte(s). Per-file cap: {} bytes; total cap: {} bytes; file cap: {}; result cap: {}. Requested seed files are prioritized; any omissions are reported as INCOMPLETE. Callee matching is lexical. Absence outside this scope is not established.",
        groups.len(), caps.file_bytes, caps.total_bytes, caps.files, caps.results,
    );
    result
}

fn note_incomplete_invocation_report(
    relative: &str,
    report: &InvocationReport,
    notes: &mut Vec<String>,
) {
    if report.parse_error_count > 0 || report.parse_status != "complete" {
        notes.push(format!(
            "canonical-call Roslyn parsed {relative} with status `{}` and {} syntax error(s)",
            report.parse_status, report.parse_error_count
        ));
    }
    if report.truncated
        || report.omitted_invocation_count > 0
        || report.omitted_argument_count > 0
    {
        notes.push(format!(
            "canonical-call Roslyn invocation evidence was partial for {relative}: {} invocation(s) and {} argument(s) omitted; {}",
            report.omitted_invocation_count,
            report.omitted_argument_count,
            report.notes.join("; ")
        ));
    }
}

fn read_bounded_vb_source(
    root: &Path,
    relative: &str,
    file_cap: u64,
    remaining: u64,
    charged_total: &mut u64,
    notes: &mut Vec<String>,
) -> Option<String> {
    if remaining == 0 {
        notes.push(format!(
            "canonical-call source budget exhausted before {relative}"
        ));
        return None;
    }
    let path = root.join(relative);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => metadata,
        _ => {
            notes.push(format!("canonical-call source unavailable or unsafe: {relative}"));
            return None;
        }
    };
    let canonical = match path.canonicalize() {
        Ok(path) if path.starts_with(root) => path,
        _ => {
            notes.push(format!("canonical-call source escaped project boundary: {relative}"));
            return None;
        }
    };
    let read_limit = (file_cap + 1).min(remaining);
    let file = match std::fs::File::open(&canonical) {
        Ok(file) => file,
        Err(error) => {
            notes.push(format!("canonical-call source read failed for {relative}: {error}"));
            return None;
        }
    };
    use std::io::Read as _;
    let mut bytes = Vec::new();
    if let Err(error) = file.take(read_limit).read_to_end(&mut bytes) {
        notes.push(format!("canonical-call source read failed for {relative}: {error}"));
        return None;
    }
    let charged = bytes.len() as u64;
    *charged_total = (*charged_total).saturating_add(charged);
    if metadata.len() > read_limit && read_limit <= file_cap {
        notes.push(format!(
            "canonical-call total source cap reached while reading {relative}"
        ));
        return None;
    }
    if charged > file_cap {
        notes.push(format!(
            "canonical-call skipped {relative}: source exceeds the {file_cap}-byte file cap"
        ));
        return None;
    }
    match String::from_utf8(bytes) {
        Ok(source) => Some(source),
        Err(_) => {
            notes.push(format!(
                "canonical-call skipped non-UTF-8 source {relative} after charging {charged} byte(s)"
            ));
            None
        }
    }
}

fn line_span_intersects(lines: &BTreeSet<u32>, start: u32, end: u32) -> bool {
    lines.range(start..=end.max(start)).next().is_some()
}

fn selector_label(selector: &ArgumentSelector) -> String {
    match selector {
        ArgumentSelector::Resolved(parameter) => {
            format!("resolved parameter ordinal {}", parameter + 1)
        }
        ArgumentSelector::Named(name) => format!("named argument `{name}`"),
        ArgumentSelector::Ordinal(ordinal) => format!("argument {}", ordinal + 1),
    }
}

fn member_has_canonical_segment(member: &str) -> bool {
    member.split('.').any(|segment| {
        let lower = segment.to_ascii_lowercase();
        ["prefix", "token", "constant", "constants"]
            .iter()
            .any(|marker| lower.contains(marker))
    })
}

fn canonical_sweep_path_excluded(path: &str) -> bool {
    let lower = path.replace('\\', "/").to_ascii_lowercase();
    let components = lower.split('/').collect::<Vec<_>>();
    components.iter().any(|component| {
        matches!(
            *component,
            ".git" | ".svn" | ".vs" | "bin" | "generated" | "obj" | "target"
                | "node_modules" | "packages" | "vendor"
        )
    }) || lower.contains(".designer.") || lower.ends_with(".g.vb")
}

fn compact_member(value: &str) -> String {
    value.chars().filter(|character| !character.is_whitespace()).collect()
}

fn normalize_vb_callee(callee: &str) -> String {
    compact_member(callee).to_ascii_lowercase()
}

fn canonical_path_key(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    if cfg!(windows) {
        normalized.to_ascii_lowercase()
    } else {
        normalized
    }
}

fn bounded_literal(value: &str) -> String {
    let mut rendered = value.chars().take(120).collect::<String>();
    if value.chars().count() > 120 {
        rendered.push_str("...");
    }
    format!("`{}`", rendered.replace('`', "\\`"))
}

/// Source-triggered runtime scenarios that graph settings/role/state edges do
/// not express. These are bounded lexical observations over a source-verified
/// snapshot. They propose tests; they do not claim that a defect exists.
pub(super) fn runtime_risk_axes(file: &str, source: &str) -> Vec<(String, String)> {
    let lower = source.to_ascii_lowercase();
    let ext = Path::new(file)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let is_markup = matches!(
        ext.as_str(),
        "aspx" | "ascx" | "master" | "html" | "htm" | "razor" | "jsx" | "tsx" | "vue"
    );
    let has_session = lower.contains("session(")
        || lower.contains("session[")
        || lower.contains("session.")
        || lower.contains("sessionstate");
    let has_view_state = lower.contains("viewstate(")
        || lower.contains("viewstate[")
        || lower.contains("viewstate.");
    let has_full_postback = lower.contains("ispostback")
        || lower.contains("__dopostback")
        || lower.contains("postbackoptions")
        || lower.contains("asp:linkbutton");
    let has_partial_postback = lower.contains("updatepanel")
        || lower.contains("asyncpostbacktrigger")
        || lower.contains("pagerequestmanager");
    let has_keyboard_or_activation = lower.contains("keydown")
        || lower.contains("keyup")
        || lower.contains("keypress")
        || lower.contains("onclick")
        || lower.contains("_click")
        || lower.contains(".click");
    let has_interactive_markup = is_markup
        && (lower.contains("<button")
            || lower.contains("<a ")
            || lower.contains("<a>")
            || lower.contains("<input")
            || lower.contains("asp:button")
            || lower.contains("asp:linkbutton")
            || lower.contains("role=\"button\"")
            || lower.contains("role='button'"));
    let optional_text = optional_text_parameters(source, &ext);
    let bound_members = if is_markup { bound_presentation_members(source) } else { Vec::new() };
    let has_localized_content = lower.contains("getdeployedcultureresxstring")
        || lower.contains("<%$ resources:")
        || lower.contains("resources.");
    let has_deferred_binding = is_markup && lower.contains("<%#");
    let has_inline_bound_output = is_markup
        && (lower.contains("<%#") || lower.contains("data-content=") || lower.contains("innerhtml"));
    let has_explicit_output_encoding = lower.contains("<%:")
        || lower.contains("htmlencode")
        || lower.contains("htmlattributeencode")
        || lower.contains("httputility.");
    let is_browser_code = matches!(
        ext.as_str(),
        "vb" | "cs" | "aspx" | "ascx" | "master" | "html" | "htm" | "razor" | "js"
            | "jsx" | "ts" | "tsx" | "vue"
    );
    let has_reinterpreted_html_payload = is_browser_code
        && (lower.contains("data-content=")
            || lower.contains("data-html=")
            || lower.contains("popover")
            || lower.contains("tooltip")
            || lower.contains("innerhtml")
            || lower.contains("insertadjacenthtml"));
    let has_spreadsheet_cell_sink = lower.contains("setcellvalue(")
        || lower.contains("xssfworkbook")
        || lower.contains("hssfworkbook")
        || lower.contains("npoi.")
        || lower.contains("exceljs")
        || lower.contains("openxml");
    let has_normalizer = ["left(", "substring(", ".trim(", "trim(", ".tolower", ".toupper"]
        .iter()
        .any(|token| lower.contains(token));
    let has_value_comparison = ["<>", "!=", "==", ".equals(", "string.equals("]
        .iter()
        .any(|token| lower.contains(token));
    let lower_file = file.to_ascii_lowercase();
    let is_generated_artifact = lower_file.contains(".designer.")
        || lower_file.ends_with(".g.cs")
        || lower_file.ends_with(".g.vb")
        || matches!(ext.as_str(), "dbml" | "generated")
        || lower.lines().take(20).any(|line| line.contains("auto-generated"));
    let bounded_storage_widths = if matches!(ext.as_str(), "sql" | "dbml") {
        let pattern = regex::Regex::new(r"(?i)\b(?:n?varchar|n?char)\s*\(\s*(\d+)\s*\)");
        pattern.map(|pattern| {
            pattern.captures_iter(source)
                .filter_map(|captures| captures.get(1).map(|value| value.as_str().to_string()))
                .take(16)
                .collect::<Vec<_>>()
        }).unwrap_or_default()
    } else {
        Vec::new()
    };
    let has_unbounded_text_storage = matches!(ext.as_str(), "sql" | "dbml")
        && regex::Regex::new(r"(?i)\b(?:n?varchar|n?text)\s*\(\s*max\s*\)")
            .is_ok_and(|pattern| pattern.is_match(source));
    let has_archive_processing = [
        "ziparchive",
        "zipfile",
        "archive.entries",
        "getentry(",
        "extractto",
        ".kmz",
        "compressedlength",
    ]
    .iter()
    .any(|token| lower.contains(token));
    let has_optional_data_context = regex::Regex::new(
        r"(?i)\b(?:optional\s+)?\w*(?:db|context)\w*\s+as\s+(?:[\w.]*data|[\w.]*db)context\b[^\r\n,)]*(?:nothing|null)?|\b(?:data|db)context\??\s+\w+\s*=\s*null\b",
    )
    .is_ok_and(|pattern| pattern.is_match(source));
    let has_unit_of_work_operation = lower.contains("submitchanges(")
        || lower.contains("savechanges(")
        || lower.contains(".dispose(")
        || lower.contains("using (")
        || lower.contains("using ");
    let has_async_browser_resource = is_browser_code
        && [
            "createobjecturl",
            "revokeobjecturl",
            "addeventlistener",
            "removeeventlistener",
            "new image(",
            "htmlimageelement",
            "getcontext(\"2d\")",
            "getcontext('2d')",
            ".onload",
            ".onerror",
        ]
        .iter()
        .any(|token| lower.contains(token));
    let has_serialized_contract = [
        "jsonproperty",
        "jsonconvert",
        "serializeobject",
        "system.text.json",
        "datacontract",
        "datamember",
    ]
    .iter()
    .any(|token| lower.contains(token))
        || matches!(ext.as_str(), "ts" | "tsx")
            && (lower.contains("interface ") || lower.contains("type "));
    let has_xml_roundtrip = [
        "xmldocument",
        "xdocument",
        "xmlreader",
        "xmlwriter",
        "loadxml(",
        "document.save(",
        ".save(writer",
    ]
    .iter()
    .any(|token| lower.contains(token));

    let mut axes = Vec::new();
    if has_session && has_view_state {
        axes.push((
            "Cross-window Session/ViewState coherence".into(),
            format!(
                "{file}: open two windows in one authenticated session; change the shared preference/state in one, then full-postback the stale window and verify rendered state, ViewState and Session converge without restoring the stale value"
            ),
        ));
    } else if has_session {
        axes.push((
            "Shared-session navigation and multi-window state".into(),
            format!(
                "{file}: change the Session-backed behavior, navigate away/back and exercise a second window in the same session; verify both consumers observe the intended state"
            ),
        ));
    }
    if has_view_state || has_full_postback {
        axes.push((
            "Initial load and full-postback reconstruction".into(),
            format!(
                "{file}: compare initial load with a full postback; verify selection, visibility, grouping and navigation state are reconstructed from the intended authority"
            ),
        ));
    }
    if has_partial_postback {
        axes.push((
            "Partial-postback refresh and handler rebinding".into(),
            format!(
                "{file}: trigger every relevant async postback and verify updated DOM state plus client handlers after one and repeated partial refreshes"
            ),
        ));
    }
    if has_keyboard_or_activation || has_interactive_markup {
        axes.push((
            "Keyboard activation parity".into(),
            format!(
                "{file}: focus each changed interactive control and exercise Enter and Space where its native role requires them; verify one activation, correct navigation/action, and no global handler suppresses native behavior"
            ),
        ));
    }
    if has_interactive_markup {
        axes.push((
            "DOM accessible names for interactive controls".into(),
            format!(
                "{file}: inspect the accessibility tree and require a non-empty computed name for every changed link, button and input, including icon-only controls"
            ),
        ));
    }
    if has_deferred_binding {
        axes.push((
            "Deferred binding lifecycle and refresh parity".into(),
            format!("{file}: WebForms-style `<%# ... %>` binding detected; prove every bound property is populated before its first consumer in Init/Load and after explicit refresh calls. Exercise initial request and postback, and identify the Page/control DataBind call instead of assuming declarative assignment has executed"),
        ));
    }
    if has_inline_bound_output && !has_explicit_output_encoding {
        axes.push((
            "Bound-value output-context safety".into(),
            format!("{file}: bound or HTML-attribute output was found without an adjacent explicit encoding signal; exercise quotes, angle brackets, ampersands, Unicode and line breaks in text and attribute contexts, and verify the framework control or formatter encodes for that exact sink"),
        ));
    }
    if has_reinterpreted_html_payload {
        axes.push((
            "Multi-stage browser/plugin output context".into(),
            format!("{file}: a value crosses an HTML attribute, DOM property, or widget boundary (`data-content`, popover/tooltip, innerHTML or equivalent). Trace server encoding through browser entity decoding and the plugin's final text-versus-HTML insertion mode. Encoding for an intermediate attribute is not proof of safety at the terminal sink; verify the rendered DOM with markup-shaped input"),
        ));
    }
    if has_spreadsheet_cell_sink {
        axes.push((
            "Spreadsheet terminal value limit".into(),
            format!("{file}: spreadsheet cell output detected; reconcile every upstream text/storage maximum with the target format and library limit. For Excel text cells, exercise 32,767 characters and 32,768 characters plus Unicode, then apply the contract's explicit truncate, reject, split or reference behavior before calling the cell writer"),
        ));
    }
    if !optional_text.is_empty() {
        let names = bounded_names(optional_text.clone());
        axes.push((
            "Optional text-input omission semantics".into(),
            format!("{file}: defaulted text parameter candidates [{names}]; exercise argument omitted, explicit null/Nothing, empty, whitespace, unchanged and a changed value. Verify preserve/clear/reject behavior from the contract and ensure an omitted sibling cannot overwrite stored data"),
        ));
    }
    if optional_text.len() > 1 {
        let names = bounded_names(optional_text);
        axes.push((
            "Sibling-field isolation and multi-change side-effect cardinality".into(),
            format!("{file}: defaulted text parameter candidates [{names}]; change each independently while omitting its siblings, then change multiple fields together. Verify stored values plus the intended number and ordering of writes, audit rows, notifications and commit/submit operations"),
        ));
    }
    if has_normalizer && has_value_comparison {
        axes.push((
            "Normalize-before-comparison and side-effect parity".into(),
            format!("{file}: normalization/truncation and value comparison coexist; compare canonical stored values rather than raw inputs. Test distinct raw inputs that normalize to the same value and require no write, audit row, notification or commit unless the contract says otherwise"),
        ));
    }
    if !bound_members.is_empty() {
        let names = bounded_names(bound_members);
        axes.push((
            "Stored-to-presented value parity".into(),
            format!("{file}: source-bound presentation members [{names}]; trace legacy/default and newly introduced values through any presenter/formatter into every affected view and export. Verify no consumer bypasses the intended formatted property and that fallback output remains readable"),
        ));
    }
    if has_localized_content {
        axes.push((
            "Localization family and fallback parity".into(),
            format!("{file}: localized resource use detected; verify the complete deployed locale family, placeholder/value formatting, missing-key fallback and encoding in UI plus export/text-only consumers"),
        ));
    }
    if !bounded_storage_widths.is_empty() {
        let widths = bounded_names(bounded_storage_widths);
        axes.push((
            "Bounded storage and payload-expansion boundary".into(),
            format!("{file}: bounded character widths [{widths}] detected; trace upstream maximum lengths and any formatting/concatenation into these sinks. Test exact limit, one over, Unicode, and deferred transaction/commit failure; choose explicit truncate, reject, widen or reference semantics from the contract"),
        ));
    }
    if has_unbounded_text_storage {
        axes.push((
            "Unbounded persistence policy and downstream limits".into(),
            format!("{file}: MAX/unbounded text persistence detected; require an explicit retention, duplication, audience and volume decision, then trace the value to every bounded UI, export, message and third-party sink. Storage acceptance does not establish downstream acceptance"),
        ));
    }
    if is_generated_artifact {
        axes.push((
            "Generated-artifact provenance and synchronization".into(),
            format!("{file}: generated or tool-managed artifact detected; require the authoritative source/schema change, generator command and receipt, plus deterministic synchronization of every generated companion. A shape/XML check alone does not prove safe regeneration"),
        ));
    }
    if has_archive_processing {
        axes.push((
            "Archive resource, namespace and cleanup bounds".into(),
            format!("{file}: archive processing detected; exercise entry-count, total-expanded-byte, per-entry-byte, compression-ratio and path-depth limits; duplicate and case-colliding names; traversal/absolute paths; nested archives; allowlisted types; cancellation; and cleanup after a mid-stream rejection. Verify every extraction entry point delegates to the same bounded helper"),
        ));
    }
    if has_optional_data_context {
        axes.push((
            "Caller-owned unit-of-work boundary".into(),
            format!("{file}: a caller-supplied data context was detected; exercise owned and borrowed contexts with unrelated pending changes. A helper must submit/save, dispose, roll back, or expose those pending changes only when the API explicitly transfers ownership; verify failure and retry paths preserve the same boundary{}", if has_unit_of_work_operation { ", especially around the detected commit/disposal operation" } else { "" }),
        ));
    }
    if has_async_browser_resource {
        axes.push((
            "Browser callback and resource disposal".into(),
            format!("{file}: asynchronous browser media/listener resources detected; run repeated attach/detach and owner/route switches, then complete callbacks after disposal. Assert stable listener count, generation-token cancellation, cache invalidation, object-URL revocation, safe failure on image/canvas/CORS errors, and decoded pixel/dimension limits before allocation"),
        ));
    }
    if has_serialized_contract {
        axes.push((
            "Serialized server/client/generated contract parity".into(),
            format!("{file}: serialized or typed wire contract detected; compare exact names, types, nullability, defaults, discriminators and child collections across server serializer output, persisted representation, client DTO/readers and committed generated output. Run the repository's real type-check/build against a representative serialized fixture"),
        ));
    }
    if has_xml_roundtrip {
        axes.push((
            "XML byte-encoding round trip".into(),
            format!("{file}: XML read/write operations detected; round-trip UTF-8 with and without BOM, UTF-16 LE/BE, non-ASCII text, declarations and namespace attributes. Assert the emitted declaration matches the actual bytes and that mutation plus failure paths preserve or deliberately normalize encoding according to the contract"),
        ));
    }
    axes
}

/// Presentation-only correction for historical setting-shaped edges. Only a
/// source-proven, uniquely declared private nullable field qualifies; a real
/// resolved setting or a qualified configuration/member chain stays a setting.
pub(super) fn nullable_member_observation(
    source_code: Option<&str>,
    file: &str,
    target: &str,
) -> Option<String> {
    use regex::Regex;
    if !file.to_ascii_lowercase().ends_with(".vb") {
        return None;
    }
    let code = source_code?;
    let name = target.strip_prefix("::")?.strip_suffix(".HasValue")?;
    if !Regex::new(r"^[A-Za-z_]\w*$").ok()?.is_match(name) {
        return None;
    }
    let escaped = regex::escape(name);
    // Unrelated nested types do not invalidate an outer field. Each use must
    // still belong to the exact class containing its unique declaration.
    let type_scopes = nullable_type_scopes(code)?;
    let declarations = Regex::new(&format!(r"(?i)\b{escaped}\s+As\b")).ok()?;
    if declarations.find_iter(&code).count() != 1 {
        return None;
    }
    let declaration = Regex::new(&format!(r"(?im)^\s*Private\s+(?:Shared\s+|ReadOnly\s+)?{escaped}\s+As\s+(?:(?:Boolean|Byte|SByte|Short|UShort|Integer|UInteger|Long|ULong|Single|Double|Decimal|Date)\s*\?|(?:System\.)?Nullable\s*\(\s*Of\s+(?:Boolean|Byte|SByte|Short|UShort|Integer|UInteger|Long|ULong|Single|Double|Decimal|Date)\s*\))\s*(?:=|$)")).ok()?;
    let decl = declaration.find(&code)?;
    let name_offset = decl.start()
        + decl
            .as_str()
            .to_ascii_lowercase()
            .find(&name.to_ascii_lowercase())?;
    let scope_at = |offset| {
        type_scopes
            .iter()
            .find(|(start, end, _)| *start <= offset && offset < *end)
            .and_then(|(_, _, scope)| *scope)
    };
    let declaration_scope = scope_at(name_offset)?;
    let parameter = Regex::new(&format!(
        r"(?im)^.*\b(?:Sub|Function|Property)\s+[^\r\n]*\([^\r\n]*\b{escaped}\b"
    ))
    .ok()?;
    if parameter.is_match(&code) {
        return None;
    }
    // Also retain uncertainty for inferred locals, loop/query variables and
    // lambda parameters that need not carry a second explicit As clause.
    let shadows = Regex::new(&format!(r"(?im)\b(?:For(?:\s+Each)?|Catch|Using|From|Let)\s+{escaped}\b|\b(?:Function|Sub)\s*\([^\r\n)]*\b{escaped}\b")).ok()?;
    let locals = Regex::new(&format!(
        r"(?i)\b(?:Private|Dim)\s+(?:Shared\s+|ReadOnly\s+)?{escaped}\b"
    ))
    .ok()?;
    if shadows.is_match(code) || locals.find_iter(code).count() != 1 {
        return None;
    }
    let uses = Regex::new(&format!(r"(?i)\b{escaped}\s*\.\s*HasValue\b")).ok()?;
    let mut lines = Vec::new();
    let mut previous_offset = 0;
    let mut line = 1;
    for usage in uses.find_iter(&code) {
        if scope_at(usage.start()) != Some(declaration_scope) {
            return None;
        }
        // A prefix receiver would make this a different member chain.
        if code[..usage.start()].trim_end().ends_with('.') {
            return None;
        }
        line += code[previous_offset..usage.start()]
            .bytes()
            .filter(|b| *b == b'\n')
            .count();
        previous_offset = usage.start();
        if lines.last() != Some(&line) {
            lines.push(line);
        }
    }
    if lines.is_empty() {
        return None;
    }
    let decl_line = code[..name_offset].bytes().filter(|b| *b == b'\n').count() + 1;
    let omitted = lines.len().saturating_sub(8);
    let sites = lines
        .iter()
        .take(8)
        .map(|line| format!("{file}:{line}"))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "{name}.HasValue: source-declared private nullable field (declaration {file}:{decl_line}); presence check at {sites}. Not a configuration-setting identity or cross-page state proof; value origin and access outcome remain unassessed. Additional use lines omitted: {omitted}."
    ))
}

/// Line ranges and exact lexical class identity in already masked VB source.
/// Other nested type bodies remain distinct, and malformed/inline declarations
/// are withheld. This does not resolve inheritance, local variables or receivers.
fn nullable_type_scopes(code: &str) -> Option<Vec<(usize, usize, Option<usize>)>> {
    use regex::Regex;
    let opening = Regex::new(r"(?i)^[\t ]*(?:(?:Public|Private|Friend|Protected|Partial|MustInherit|NotInheritable|Shadows)\s+)*(Class|Structure|Module|Interface|Enum)\s+\S+").ok()?;
    let closing = Regex::new(r"(?i)^End[\t ]+(Class|Structure|Module|Interface|Enum)$").ok()?;
    let mut stack: Vec<(String, usize)> = Vec::new();
    let mut ranges = Vec::new();
    let mut offset = 0;
    for line in code.split_inclusive('\n') {
        if let Some(caps) = opening.captures(line) {
            if line.contains(':') || line.trim_end().ends_with('_') {
                return None;
            }
            stack.push((caps[1].to_ascii_lowercase(), offset));
        }
        let class = stack
            .last()
            .and_then(|(kind, id)| (kind == "class").then_some(*id));
        ranges.push((offset, offset + line.len(), class));
        if let Some(caps) = closing.captures(line.trim()) {
            let (kind, _) = stack.pop()?;
            if !kind.eq_ignore_ascii_case(&caps[1]) {
                return None;
            }
        }
        offset += line.len();
    }
    stack.is_empty().then_some(ranges)
}

/// A dangling target has no reliable use location merely because the graph's
/// owner is a file node. Supply a concrete, bounded recovery query instead.
pub(super) fn setting_use_recovery(pid: &str, file: &str, target: &str) -> String {
    let token = target.strip_prefix("::").unwrap_or(target);
    format!(
        "Source-use recovery: grep_project({}) (literal token search; inspect matches, not a proven setting binding).",
        serde_json::json!({"project_id":pid,"pattern":token,"path_prefix":file,"max_results":20,"output_json":true,"regex":false})
    )
}

/// Known stale graph axes are withheld. Legacy axes remain explicitly
/// unverified so a missing fingerprint cannot masquerade as current evidence.
pub(super) fn axis_source(
    graph: &engram_graph::GraphStore,
    pid: &str,
    root: &Path,
    file: &str,
    bytes_read: &mut u64,
) -> (bool, Option<String>, Option<Vec<u8>>) {
    use std::io::Read;
    let verify = || -> Result<Option<String>, String> {
        let node = graph
            .get_node(pid, &format!("file:{file}"))
            .map_err(|e| e.to_string())?;
        let metadata = node.and_then(|node| node.metadata);
        if let Some(version) = metadata
            .as_ref()
            .and_then(|meta| meta.get("source_index_version"))
            .and_then(serde_json::Value::as_u64)
        {
            if version < engram_index::SOURCE_INDEX_VERSION {
                return Err(format!(
                    "indexed axes use obsolete source format {version}; update_project must re-extract this file before its dependencies can be used"
                ));
            }
        }
        Ok(metadata.and_then(|meta| {
            meta.get("file_hash")
                .and_then(|value| value.as_str())
                .map(str::to_owned)
        }))
    };
    let hash = match verify() {
        Ok(Some(hash)) => hash,
        Ok(None) => {
            return (
                true,
                Some(format!(
                    "{file}: indexed axes are UNVERIFIED because the file fingerprint is absent"
                )),
                None,
            );
        }
        Err(error) => {
            return (
                false,
                Some(format!("{file}: axis fingerprint lookup failed: {error}")),
                None,
            );
        }
    };
    let read = || -> Result<Vec<u8>, String> {
        let path = engram_core::safe_join(root, file)
            .map_err(|e| e.to_string())?
            .canonicalize()
            .map_err(|e| e.to_string())?;
        let root = root.canonicalize().map_err(|e| e.to_string())?;
        if !path.starts_with(root) {
            return Err("source escapes the registered project".into());
        }
        let size = std::fs::metadata(&path).map_err(|e| e.to_string())?.len();
        if size > 8 * 1024 * 1024 || bytes_read.saturating_add(size) > 64 * 1024 * 1024 {
            return Err("axis verification budget exceeded (8 MiB/file, 64 MiB/query)".into());
        }
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .map_err(|e| e.to_string())?
            .take(8 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() > 8 * 1024 * 1024
            || bytes_read.saturating_add(bytes.len() as u64) > 64 * 1024 * 1024
        {
            return Err("axis verification budget exceeded while reading".into());
        }
        Ok(bytes)
    };
    match read() {
        Ok(bytes) => {
            *bytes_read += bytes.len() as u64;
            // file_hash is the indexer's raw-byte fingerprint. ContentHash
            // normalizes line endings for chunks and falsely rejects CRLF files.
            if blake3::hash(&bytes).to_hex().as_str() == hash {
                (true, None, Some(bytes))
            } else {
                (
                    false,
                    Some(format!(
                        "{file}: STALE graph axes withheld because source changed; run update_project before deriving indexed axes"
                    )),
                    None,
                )
            }
        }
        Err(error) => (
            false,
            Some(format!(
                "{file}: axis source verification unavailable: {error}"
            )),
            None,
        ),
    }
}

/// Resolve only an explicit code-behind directive. Markup requests use their
/// already verified snapshot. Code-behind requests scan a bounded sibling
/// directory for markup that explicitly declares the exact source path; the
/// caller must freshness-verify that returned markup before using the inverse
/// relationship.
pub(super) fn declared_axis_companion(
    root: &Path,
    file: &str,
    snapshot: &[u8],
) -> Result<Option<(String, bool)>, String> {
    let extension = Path::new(file).extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    let is_markup = matches!(extension.as_deref(), Some("aspx" | "ascx" | "master"));
    if !is_markup {
        // Follow code-behind to markup only when that existing markup
        // explicitly declares this exact source file. The caller separately
        // freshness-verifies the markup before rendering its axes/relation.
        if !matches!(extension.as_deref(), Some("vb" | "cs")) {
            return Ok(None);
        }
        let root = root.canonicalize().map_err(|e| e.to_string())?;
        let source_file = engram_core::safe_join(&root, file).map_err(|e| e.to_string())?
            .canonicalize().map_err(|e| format!("code-behind {file} is unavailable: {e}"))?;
        let Some(parent) = source_file.parent() else { return Ok(None); };
        let mut candidates = std::fs::read_dir(parent).map_err(|e| e.to_string())?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| matches!(path.extension().and_then(|value| value.to_str())
                .map(str::to_ascii_lowercase).as_deref(), Some("aspx" | "ascx" | "master")))
            .collect::<Vec<_>>();
        candidates.sort();
        if candidates.len() > 256 {
            return Err(format!("code-behind sibling markup scan truncated at 256 of {} files", candidates.len()));
        }
        let mut scanned_bytes = 0_u64;
        for markup_file in candidates {
            let Ok(markup_file) = markup_file.canonicalize() else { continue; };
            let Ok(relative) = markup_file.strip_prefix(&root) else { continue; };
            let Ok(metadata) = std::fs::metadata(&markup_file) else { continue; };
            if !metadata.is_file() || metadata.len() > 4 * 1024 * 1024 { continue; }
            scanned_bytes = scanned_bytes.saturating_add(metadata.len());
            if scanned_bytes > 4 * 1024 * 1024 {
                return Err("code-behind sibling markup scan exceeded 4 MiB; narrow the requested files".into());
            }
            let Ok(markup_bytes) = std::fs::read(&markup_file) else { continue; };
            let Ok(markup) = std::str::from_utf8(&markup_bytes) else { continue; };
            let Some(declared) = crate::services::validation_mapping_service::declared_codebehind(markup) else { continue; };
            let declared = declared.replace('\\', "/");
            let declared_file = if let Some(relative) = declared.strip_prefix("~/") {
                super::access_layer_tools::discover_web_application_root(&root, &markup_file).join(relative)
            } else {
                markup_file.parent().unwrap_or(&root).join(&declared)
            };
            let Ok(declared_file) = declared_file.canonicalize() else { continue; };
            if declared_file == source_file {
                return Ok(Some((relative.to_string_lossy().replace('\\', "/"), false)));
            }
        }
        return Ok(None);
    }
    let markup = std::str::from_utf8(snapshot).map_err(|_| {
        "markup encoding is not UTF-8; code-behind declaration not examined".to_string()
    })?;
    let Some(declared) = crate::services::validation_mapping_service::declared_codebehind(markup)
    else {
        return Ok(None);
    };
    let declared = declared.replace('\\', "/");
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let page = engram_core::safe_join(&root, file).map_err(|e| e.to_string())?;
    let candidate = if let Some(relative) = declared.strip_prefix("~/") {
        super::access_layer_tools::discover_web_application_root(&root, &page).join(relative)
    } else {
        page.parent().unwrap_or(&root).join(&declared)
    };
    let candidate = candidate
        .canonicalize()
        .map_err(|e| format!("declared code-behind {declared} is unavailable: {e}"))?;
    let relative = candidate
        .strip_prefix(&root)
        .map_err(|_| "declared code-behind escapes the registered project".to_string())?;
    if !candidate.is_file()
        || !matches!(
            candidate
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("vb" | "cs")
        )
    {
        return Err(format!(
            "declared code-behind {declared} is not a supported VB/C# source file"
        ));
    }
    Ok(Some((relative.to_string_lossy().replace('\\', "/"), true)))
}

/// Return markup files that directly register the requested user control.
/// This is a single graph hop over source-extracted `registers_control` edges;
/// callers still freshness-verify every returned host before using it.
pub(super) fn registered_control_hosts(
    graph: &engram_graph::GraphStore,
    project_id: &str,
    file: &str,
) -> Result<(Vec<String>, bool), String> {
    if !file.to_ascii_lowercase().ends_with(".ascx") {
        return Ok((Vec::new(), false));
    }
    let node_id = format!("file:{}", file.replace('\\', "/"));
    let incoming = graph.find_incoming_edges_with_kind(
        project_id,
        Some(engram_graph::EdgeKind::RegistersControl),
        &node_id,
        33,
    ).map_err(|error| format!("registered-control host lookup failed: {error}"))?;
    let truncated = incoming.len() > 32;
    let mut hosts = incoming.into_iter().take(32)
        .filter_map(|(source, _, _)| {
            source.strip_prefix("page:").or_else(|| source.strip_prefix("file:"))
                .map(str::to_string)
        })
        .filter(|path| matches!(
            Path::new(path).extension().and_then(|value| value.to_str())
                .map(str::to_ascii_lowercase).as_deref(),
            Some("aspx" | "ascx" | "master")
        ))
        .collect::<Vec<_>>();
    hosts.sort();
    hosts.dedup();
    Ok((hosts, truncated))
}

pub(super) struct RuleCase {
    pub requirement: String,
    pub doc_id: String,
    pub source_status: String,
    pub source_warnings: Vec<String>,
    pub expected_outcome_status: String,
    pub other_outcome_status: String,
    pub source_diagnostic_qualification: String,
    pub reaching_prerequisites_require_review: bool,
    pub reaching_qualification: String,
    pub outcome_dependencies:
        Vec<crate::services::business_outcome_dependencies::OutcomeDependency>,
}

/// Warnings describe the analyzed document. Their rule numbers are local to
/// that document, not the case numbers assigned by a multi-document matrix.
fn source_check_warnings(document: &str) -> Vec<String> {
    let mut in_checks = false;
    let mut warnings: Vec<String> = Vec::new();
    for line in document.lines() {
        if line.starts_with("_Source: ") {
            in_checks = false;
        }
        if line.starts_with("## ") {
            in_checks = line.trim() == "## Source checks requiring review";
            continue;
        }
        if !in_checks || line.trim().is_empty() {
            continue;
        }
        if let Some(warning) = line.trim().strip_prefix("- ") {
            warnings.push(warning.to_string());
        } else if let Some(warning) = warnings.last_mut() {
            warning.push('\n');
            warning.push_str(line.trim());
        } else {
            warnings.push(line.trim().to_string());
        }
    }
    let total = warnings.len();
    warnings.truncate(20);
    for warning in &mut warnings {
        if warning.chars().count() > 1000 {
            *warning = warning.chars().take(1000).collect::<String>()
                + " [warning truncated; fetch full source document]";
        }
    }
    if total > 20 {
        warnings.push(format!(
            "{} additional source-check warnings omitted; fetch full source document",
            total - 20
        ));
    }
    warnings
}

pub(super) fn collect(
    search: &HybridSearchEngine,
    pid: &str,
    generation: u64,
    files: &[String],
    root: &Path,
    max_cases: usize,
) -> Result<(Vec<RuleCase>, Vec<String>), String> {
    let query = HybridQuery {
        project_id: pid.into(),
        namespace: "business_logic".into(),
        generation,
        text: "*".into(),
        top_k: 201,
        fts_mode: "regex".into(),
        include_path_prefixes: Some(
            files
                .iter()
                .map(|file| format!("__business_logic/{file}/"))
                .collect(),
        ),
        exclude_path_prefixes: None,
        include_path_suffixes: None,
        language_filters: None,
        author_filter: None,
        date_after: None,
        date_before: None,
        use_mmr: false,
    };
    let mut hits = search
        .lexical_search(&query)
        .map_err(|error| error.to_string())?;
    let mut notes = Vec::new();
    if hits.len() > 200 {
        notes.push("rule document coverage truncated at 200; narrow the requested files".into());
        hits.truncate(200);
    }
    let mut seen = HashSet::new();
    let mut audit = SourceAudit::default();
    let mut cases = Vec::new();
    let mut outcome_audit = crate::services::business_outcome_dependencies::OutcomeAudit::default();
    for hit in hits {
        if !seen.insert(hit.doc_id.clone()) {
            continue;
        }
        let Some((_, _, document, _, _)) = search
            .get_doc_by_doc_id(pid, "business_logic", 0, &hit.doc_id)
            .map_err(|error| error.to_string())?
        else {
            notes.push(format!("{}: full rule document unavailable", hit.doc_id));
            continue;
        };
        let source = document.lines().find_map(|line| {
            line.strip_prefix("_Source: ")
                .and_then(|source| source.strip_suffix('_'))
        });
        if !source.is_some_and(|source| files.iter().any(|file| file == &source.replace('\\', "/")))
        {
            notes.push(format!(
                "{}: rule source does not match requested file scope",
                hit.doc_id
            ));
            continue;
        }
        let source_warnings = source_check_warnings(&document);
        let rule_diagnostics = crate::services::business_rule_diagnostics::from_document(&document);
        let warning_notes = || {
            source_warnings.iter().map(|warning| format!(
            "{}: analysis source check (document-local rule numbers): {warning}; recovery: get_chunk(doc_id=\"{}\", namespace=\"business_logic\")",
            hit.doc_id, hit.doc_id,
        )).collect::<Vec<_>>()
        };
        let status = audit.describe(&document, root);
        if !status.starts_with("VERIFIED_METHOD_HASH:") {
            notes.push(format!("{}: {status}", hit.doc_id));
            notes.extend(warning_notes());
            continue;
        }
        let outcome_evidence =
            crate::services::business_outcome_dependencies::from_document(&document);
        // Independent source alternatives; never attach them to inferred rules by proximity.
        // Eight paths per document, exact source/anchor checks, remaining paths recoverable.
        notes.extend(
            audit
                .return_path_checklist(&document, root, 8)
                .into_iter()
                .map(|note| format!("{}: {note}", hit.doc_id)),
        );
        notes.push(format!(
            "{}: full_document: get_chunk({})",
            hit.doc_id,
            serde_json::json!({"project_id":pid,"namespace":"business_logic","doc_id":hit.doc_id})
        ));
        notes.extend(
            super::business_source::claim_review_guidance(pid, &hit.doc_id, &document)
                .into_iter()
                .map(|note| format!("{}: {note}", hit.doc_id)),
        );
        let cases_before_document = cases.len();
        let exact_rules = crate::services::business_rule_diagnostics::exact_document_rules(
            rule_diagnostics.as_ref(),
            &document,
        );
        if exact_rules.is_none() && rule_diagnostics.is_some() {
            notes.push(format!("{}: exact stored rule-boundary recovery unavailable (legacy/mismatched/ambiguous block or 400-rule/128-KiB recovery budget); using legacy line display without guessing diagnostic identity. Full rules: get_chunk for this document.", hit.doc_id));
        }
        let rules = exact_rules.unwrap_or_else(|| {
            let mut in_rules = false;
            document
                .lines()
                .filter_map(|line| {
                    if line.starts_with("## ") {
                        in_rules = line.trim() == "## Business Rules";
                        return None;
                    }
                    if !in_rules {
                        return None;
                    }
                    let rule = line.trim().strip_prefix("- ")?;
                    (!rule.trim().is_empty()).then(|| rule.to_owned())
                })
                .collect()
        });
        for rule in &rules {
            // Bound each document before global balancing. Business-rule
            // documents can contain hundreds of lines; eight candidates retain
            // local variety while preventing one method from monopolizing the
            // final matrix.
            if cases.len() - cases_before_document >= 8 {
                notes.push(format!(
                    "{}: source-linked case candidates truncated at 8 for document balance",
                    hit.doc_id
                ));
                break;
            }
            let mut requirement: String = rule.chars().take(1000).collect();
            if rule.chars().count() > 1000 {
                requirement.push_str(" [truncated; fetch source document]");
            }
            let (expected_outcome_status, outcome_dependencies) =
                crate::services::business_outcome_dependencies::for_rule_with_audit(
                    outcome_evidence.as_ref(),
                    rule,
                    root,
                    &mut outcome_audit,
                );
            let reaching = crate::services::business_reaching_context::qualify(
                outcome_evidence
                    .as_ref()
                    .and_then(|e| e.reaching_context.as_ref()),
                Some(rule),
            );
            let expected_outcome_status = if reaching.has_prerequisites
                && expected_outcome_status != "blocked_pending_helper_outcome"
            {
                "blocked_pending_reaching_prerequisite_review".to_owned()
            } else {
                expected_outcome_status
            };
            let source_diagnostic = crate::services::business_rule_diagnostics::associate(
                rule_diagnostics.as_ref(),
                rule,
            );
            let other_outcome_status = expected_outcome_status.clone();
            let expected_outcome_status = if source_diagnostic.blocks_outcome {
                "blocked_pending_source_validation".to_owned()
            } else {
                expected_outcome_status
            };
            cases.push(RuleCase {
                requirement,
                doc_id: hit.doc_id.clone(),
                source_status: status.clone(),
                source_warnings: source_warnings.clone(),
                expected_outcome_status,
                other_outcome_status,
                source_diagnostic_qualification: source_diagnostic.summary,
                reaching_prerequisites_require_review: reaching.has_prerequisites,
                reaching_qualification: reaching.summary,
                outcome_dependencies,
            });
        }
        if cases.len() == cases_before_document {
            notes.extend(warning_notes());
        }
    }
    let candidate_count = cases.len();
    let mut groups: Vec<(String, VecDeque<RuleCase>)> = Vec::new();
    for case in cases {
        if let Some((_, queue)) = groups.iter_mut().find(|(id, _)| id == &case.doc_id) {
            queue.push_back(case);
        } else {
            groups.push((case.doc_id.clone(), VecDeque::from([case])));
        }
    }
    let mut balanced = Vec::with_capacity(candidate_count.min(max_cases));
    while balanced.len() < max_cases {
        let mut advanced = false;
        for (_, queue) in &mut groups {
            if let Some(case) = queue.pop_front() {
                balanced.push(case);
                advanced = true;
                if balanced.len() == max_cases {
                    break;
                }
            }
        }
        if !advanced {
            break;
        }
    }
    if candidate_count > balanced.len() {
        notes.push(format!(
            "source-linked cases sampled round-robin across {} evidence document(s): showing {} of {} candidates (max_source_cases={})",
            groups.len(),
            balanced.len(),
            candidate_count,
            max_cases
        ));
    }
    Ok((balanced, notes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::{Config, ContentHash, RelPath};
    use engram_index::IndexDoc;

    #[test]
    fn runtime_axes_cover_cross_window_postback_keyboard_and_names() {
        let source = r#"
            <%@ Page Language="VB" %>
            <asp:UpdatePanel runat="server">
              <ContentTemplate>
                <asp:LinkButton ID="choose" runat="server" OnClick="choose_Click"><i class="icon"></i></asp:LinkButton>
              </ContentTemplate>
            </asp:UpdatePanel>
            <% If Session("mode") IsNot Nothing AndAlso ViewState("mode") IsNot Nothing Then %>
        "#;
        let axes = runtime_risk_axes("Pages/Choose.aspx", source);
        let labels = axes
            .iter()
            .map(|(label, _)| label.as_str())
            .collect::<Vec<_>>();
        assert!(labels.contains(&"Cross-window Session/ViewState coherence"));
        assert!(labels.contains(&"Initial load and full-postback reconstruction"));
        assert!(labels.contains(&"Partial-postback refresh and handler rebinding"));
        assert!(labels.contains(&"Keyboard activation parity"));
        assert!(labels.contains(&"DOM accessible names for interactive controls"));
        assert!(
            axes
                .iter()
                .all(|(_, evidence)| evidence.contains("Pages/Choose.aspx"))
        );
    }

    #[test]
    fn runtime_axes_separate_omitted_empty_and_sibling_text_updates() {
        let source = r#"
Public Shared Function UpdateRow(Optional name As String = "", Optional notes As String = Nothing, Optional coordinates As String = "") As Boolean
    Return True
End Function
"#;
        let axes = runtime_risk_axes("Domain/Writer.vb", source);
        let joined = axes.iter().map(|(axis, evidence)| format!("{axis}: {evidence}"))
            .collect::<Vec<_>>().join("\n");
        assert!(joined.contains("Optional text-input omission semantics"), "{joined}");
        assert!(joined.contains("Sibling-field isolation"), "{joined}");
        for expected in ["name", "notes", "coordinates", "omitted", "empty", "whitespace"] {
            assert!(joined.contains(expected), "missing {expected}: {joined}");
        }
    }

    #[test]
    fn markup_axes_trace_bound_values_and_localization() {
        let source = r#"
<asp:BoundField DataField="raw_text" HeaderText="<%$ Resources: text, Event %>" />
<%# Eval("FormattedText") %>
"#;
        let axes = runtime_risk_axes("Pages/Events.ascx", source);
        let joined = axes.iter().map(|(axis, evidence)| format!("{axis}: {evidence}"))
            .collect::<Vec<_>>().join("\n");
        assert!(joined.contains("Stored-to-presented value parity"), "{joined}");
        assert!(joined.contains("FormattedText") && joined.contains("raw_text"), "{joined}");
        assert!(joined.contains("Localization family and fallback parity"), "{joined}");
    }

    #[test]
    fn markup_axes_cover_deferred_binding_and_unproven_output_encoding() {
        let source = r#"
<uc:Events runat="server" TablePrefix="<%# Model.Prefix %>" />
<span data-content='<%# Eval("RawText") %>'></span>
"#;
        let axes = runtime_risk_axes("Pages/Events.aspx", source);
        let joined = axes.iter().map(|(axis, evidence)| format!("{axis}: {evidence}"))
            .collect::<Vec<_>>().join("\n");
        assert!(joined.contains("Deferred binding lifecycle"), "{joined}");
        assert!(joined.contains("Page/control DataBind"), "{joined}");
        assert!(joined.contains("Bound-value output-context safety"), "{joined}");
        assert!(joined.contains("quotes") && joined.contains("attribute contexts"), "{joined}");
    }

    #[test]
    fn runtime_axes_follow_reinterpreted_html_to_the_terminal_sink() {
        let source = r#"Return HtmlAttributeEncode(value) + "<a class='popover' data-content='...'>""#;
        let axes = runtime_risk_axes("Presenters/EventText.vb", source);
        let joined = axes.iter().map(|(axis, evidence)| format!("{axis}: {evidence}"))
            .collect::<Vec<_>>().join("\n");
        assert!(joined.contains("Multi-stage browser/plugin output context"), "{joined}");
        assert!(joined.contains("browser entity decoding"), "{joined}");
        assert!(joined.contains("terminal sink"), "{joined}");
    }

    #[test]
    fn runtime_axes_cover_excel_cell_and_unbounded_storage_limits() {
        let export = runtime_risk_axes(
            "Reports/Export.vb",
            "Imports NPOI.XSSF.UserModel\ncell.SetCellValue(row.DisplayText)",
        );
        let export_text = export.iter().map(|(axis, evidence)| format!("{axis}: {evidence}"))
            .collect::<Vec<_>>().join("\n");
        assert!(export_text.contains("Spreadsheet terminal value limit"), "{export_text}");
        assert!(export_text.contains("32,767") && export_text.contains("32,768"), "{export_text}");

        let schema = runtime_risk_axes(
            "Database/Events.sql",
            "[complete_value] NVARCHAR(MAX) NULL",
        );
        let schema_text = schema.iter().map(|(axis, evidence)| format!("{axis}: {evidence}"))
            .collect::<Vec<_>>().join("\n");
        assert!(schema_text.contains("Unbounded persistence policy"), "{schema_text}");
        assert!(schema_text.contains("retention") && schema_text.contains("downstream"), "{schema_text}");
    }

    #[test]
    fn intent_axes_turn_approved_literal_decisions_into_hard_assertions() {
        let axes = intent_risk_axes(Some(
            "Approved decision: Entity=0, Name=1, Type=2 reserved; field_id INT NOT NULL DEFAULT 0 is required",
        ));
        let joined = axes.iter().map(|(axis, evidence)| format!("{axis}: {evidence}"))
            .collect::<Vec<_>>().join("\n");
        assert!(joined.contains("Supplied contract invariant fidelity"), "{joined}");
        assert!(joined.contains("machine-checkable assertion"), "{joined}");
        assert!(joined.contains("shifted identifiers") && joined.contains("nullability"), "{joined}");
    }

    #[test]
    fn unapproved_intent_is_never_labelled_as_approved() {
        let axes = intent_risk_axes(Some(
            "UNAPPROVED contract checkpoint pending: field_id=2 is a candidate invariant",
        ));
        let joined = axes
            .iter()
            .map(|(axis, evidence)| format!("{axis}: {evidence}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("Supplied contract invariant fidelity"), "{joined}");
        assert!(!joined.to_ascii_lowercase().contains("approved change intent"), "{joined}");
        assert!(joined.contains("until the contract is human-approved"), "{joined}");
    }

    #[test]
    fn source_axes_cover_normalization_storage_width_and_generator_provenance() {
        let mutation = "If row.Name <> input Then row.Name = Left(input, 100)";
        let mutation_axes = runtime_risk_axes("Domain/Writer.vb", mutation);
        assert!(mutation_axes.iter().any(|(axis, evidence)|
            axis.contains("Normalize-before-comparison") && evidence.contains("same value")));

        let schema = "<Column DbType=\"NVarChar(256) NOT NULL\" />";
        let schema_axes = runtime_risk_axes("Model/Store.dbml", schema);
        let joined = schema_axes.iter().map(|(axis, evidence)| format!("{axis}: {evidence}"))
            .collect::<Vec<_>>().join("\n");
        assert!(joined.contains("Bounded storage") && joined.contains("256"), "{joined}");
        assert!(joined.contains("Generated-artifact provenance"), "{joined}");
        assert!(joined.contains("generator command"), "{joined}");
    }

    #[test]
    fn intent_axes_cover_archive_derived_state_browser_lifecycle_and_wire_contracts() {
        let axes = intent_risk_axes(Some(
            "Convert uploaded KML/KMZ archives lazily into derived JSON; cache tinted icons and update the generated JavaScript DTO interface",
        ));
        let joined = axes
            .iter()
            .map(|(axis, evidence)| format!("{axis}: {evidence}"))
            .collect::<Vec<_>>()
            .join("\n");
        for expected in [
            "Bounded archive processing contract",
            "Persistent derived-state lifecycle",
            "Asynchronous browser-resource lifecycle",
            "End-to-end wire-contract parity",
            "XML encoding and declaration round trip",
        ] {
            assert!(joined.contains(expected), "missing {expected}: {joined}");
        }
        assert!(joined.contains("entry count") && joined.contains("expanded bytes"), "{joined}");
        assert!(joined.contains("simultaneous first readers") && joined.contains("version invalidation"), "{joined}");
        assert!(joined.contains("object-URL revocation") && joined.contains("listener cardinality"), "{joined}");
        assert!(joined.contains("representative serialized fixture"), "{joined}");
        assert!(joined.contains("UTF-16 LE/BE") && joined.contains("reparsed"), "{joined}");
    }

    #[test]
    fn source_axes_cover_archive_context_browser_and_serialized_contract_risks() {
        let archive = runtime_risk_axes(
            "Conversion/ArchiveReader.cs",
            "using var zip = new ZipArchive(stream); foreach (var entry in zip.Entries) { entry.ExtractToFile(path); }",
        );
        let context = runtime_risk_axes(
            "Data/Writer.vb",
            "Public Shared Sub Save(Optional db As AppDataContext = Nothing)\n db.SubmitChanges()\nEnd Sub",
        );
        let browser = runtime_risk_axes(
            "client/iconCache.ts",
            "const image = new Image(); image.onload = draw; target.addEventListener('load', draw); return URL.createObjectURL(blob);",
        );
        let wire = runtime_risk_axes(
            "client/contracts.ts",
            "export interface LayerDto { status?: string; children: LayerDto[]; }",
        );
        let xml = runtime_risk_axes(
            "Conversion/XmlUpdater.vb",
            "Dim document As New XmlDocument()\n document.LoadXml(text)\n document.Save(writer)",
        );
        let render = |axes: Vec<(String, String)>| {
            axes
                .iter()
                .map(|(axis, evidence)| format!("{axis}: {evidence}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let archive = render(archive);
        let context = render(context);
        let browser = render(browser);
        let wire = render(wire);
        let xml = render(xml);
        assert!(archive.contains("Archive resource, namespace and cleanup bounds"), "{archive}");
        assert!(archive.contains("case-colliding") && archive.contains("mid-stream rejection"), "{archive}");
        assert!(context.contains("Caller-owned unit-of-work boundary"), "{context}");
        assert!(context.contains("unrelated pending changes") && context.contains("commit/disposal"), "{context}");
        assert!(browser.contains("Browser callback and resource disposal"), "{browser}");
        assert!(browser.contains("generation-token") && browser.contains("pixel/dimension"), "{browser}");
        assert!(wire.contains("Serialized server/client/generated contract parity"), "{wire}");
        assert!(wire.contains("type-check/build") && wire.contains("child collections"), "{wire}");
        assert!(xml.contains("XML byte-encoding round trip"), "{xml}");
        assert!(xml.contains("UTF-16 LE/BE") && xml.contains("actual bytes"), "{xml}");
    }

    #[test]
    fn configured_risk_packs_hot_reload_and_project_ids_override_global() {
        let temp = tempfile::TempDir::new().unwrap();
        let data = temp.path().join("data");
        let project = temp.path().join("project");
        std::fs::create_dir_all(data.join("rules")).unwrap();
        std::fs::create_dir_all(project.join(".engram")).unwrap();
        std::fs::write(data.join("rules/test-risk-rules.yaml"), r#"
version: 1
rules:
  - id: team.no-blocking-wait
    title: Global wait rule
    guidance: Verify the asynchronous alternative and cancellation behavior.
    extensions: [vb]
    any_terms: [".Wait()"]
  - id: team.no-inline-if
    title: Single-line conditional convention
    guidance: Expand the conditional according to the repository convention.
    extensions: [vb]
    any_terms: [" Then Return "]
"#).unwrap();
        std::fs::write(project.join(".engram/test-risk-rules.yaml"), r#"
version: 1
rules:
  - id: team.no-blocking-wait
    title: Repository wait rule v1
    guidance: Use this repository's asynchronous helper and verify cancellation.
    extensions: [.vb]
    any_terms: [".Wait()"]
"#).unwrap();

        let (pack, notes) = load_configured_risk_pack(&data, &project);
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(pack.len(), 2);
        let first = configured_risk_axes(&pack, "Site/Worker.vb", "task.Wait()");
        assert_eq!(first.len(), 1, "{first:?}");
        assert_eq!(first[0].0, "Repository wait rule v1");
        assert!(first[0].1.contains("project pack"), "{:?}", first[0]);

        std::fs::write(project.join(".engram/test-risk-rules.yaml"), r#"
version: 1
rules:
  - id: team.no-blocking-wait
    title: Repository wait rule v2
    guidance: Reloaded without a daemon restart.
    extensions: [vb]
    any_terms: [".Wait()"]
"#).unwrap();
        let (reloaded, notes) = load_configured_risk_pack(&data, &project);
        assert!(notes.is_empty(), "{notes:?}");
        let second = configured_risk_axes(&reloaded, "Site/Worker.vb", "task.Wait()");
        assert_eq!(second[0].0, "Repository wait rule v2");
        assert!(second[0].1.contains("Reloaded without a daemon restart"));
    }

    #[test]
    fn invalid_risk_pack_is_reported_and_never_partially_applied() {
        let temp = tempfile::TempDir::new().unwrap();
        let data = temp.path().join("data");
        let project = temp.path().join("project");
        std::fs::create_dir_all(data.join("rules")).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(data.join("rules/test-risk-rules.yaml"), r#"
version: 1
rules:
  - id: valid-looking-rule
    title: This must not be partially loaded
    guidance: Reject the complete pack when any rule is invalid.
    extensions: [vb]
    any_terms: ["Execute("]
  - id: invalid-rule
    title: Unknown fields are rejected
    guidance: This pack is invalid.
    extensions: [vb]
    any_terms: ["Execute("]
    executable: powershell.exe
"#).unwrap();

        let (pack, notes) = load_configured_risk_pack(&data, &project);
        assert_eq!(pack.len(), 0);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("invalid YAML/schema"), "{notes:?}");
        assert!(configured_risk_matches(&pack, "Worker.vb", "Execute(input)").is_empty());
    }

    #[test]
    fn historical_cutoff_excludes_newer_and_undated_configured_rules() {
        let temp = tempfile::TempDir::new().unwrap();
        let data = temp.path().join("data");
        let project = temp.path().join("project");
        std::fs::create_dir_all(data.join("rules")).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(data.join("rules/test-risk-rules.yaml"), r#"
version: 1
provenance: review corpus
rules:
  - id: before
    title: Before cutoff
    guidance: This rule is available to the replay.
    introduced_at: 2026-01-01
    any_terms: ["Execute("]
  - id: after
    title: After cutoff
    guidance: This rule must not leak backwards.
    introduced_at: 2026-09-14
    any_terms: ["Execute("]
  - id: undated
    title: Undated
    guidance: Undated knowledge is not replay-safe.
    any_terms: ["Execute("]
"#).unwrap();

        let (pack, notes) = load_configured_risk_pack_before(
            &data, &project, Some("2026-09-03"),
        );
        assert_eq!(pack.len(), 1);
        assert_eq!(notes.len(), 2, "{notes:?}");
        let matched = configured_risk_matches(&pack, "Worker.vb", "Execute(input)");
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].id, "before");
        assert_eq!(matched[0].introduced_at.as_deref(), Some("2026-01-01"));
        assert_eq!(matched[0].provenance.as_deref(), Some("review corpus"));
    }

    #[test]
    fn intent_axes_are_explicitly_change_derived_and_cover_migrations() {
        let axes = intent_risk_axes(Some("Add individual field-level event logging and use one canonical prefix constant"));
        let joined = axes.iter().map(|(axis, evidence)| format!("{axis}: {evidence}"))
            .collect::<Vec<_>>().join("\n");
        assert!(joined.contains("Discriminator compatibility"), "{joined}");
        assert!(joined.contains("Canonical-token migration"), "{joined}");
        assert!(joined.contains("Event cardinality"), "{joined}");
        assert!(joined.contains("persistence-to-presentation"), "{joined}");
        assert!(joined.contains("writer culture"), "{joined}");
        assert!(joined.contains("supplied change intent"), "{joined}");
        assert!(!joined.contains("approved change intent"), "{joined}");
        assert!(intent_risk_axes(None).is_empty());
    }

    fn added_diff(
        path: &str,
        lines: &[(usize, &str)],
    ) -> crate::services::pre_commit_review_service::DiffFile {
        crate::services::pre_commit_review_service::DiffFile {
            path: path.into(),
            change_type: crate::services::pre_commit_review_service::ChangeType::Modified,
            added_lines: lines.iter().map(|(n, text)| (*n, (*text).into())).collect(),
            removed_lines: Vec::new(),
            added_content: lines.iter().map(|(_, text)| *text).collect::<Vec<_>>().join("\n"),
            removed_content: String::new(),
            hunks: Vec::new(),
            is_binary: false,
        }
    }

    fn span(source: &str, text: &str) -> (u32, u32) {
        let start = source.find(text).unwrap();
        (start as u32, text.encode_utf16().count() as u32)
    }

    fn argument(
        source: &str,
        text: &str,
        line: u32,
        name: Option<&str>,
        classification: &str,
        ordinal: u32,
    ) -> engram_index::vb_extractor::VbInvocationArgument {
        let (span_start, span_length) = span(source, text);
        engram_index::vb_extractor::VbInvocationArgument {
            source_index: 0,
            span_start,
            span_length,
            start_line: line,
            end_line: line,
            name: name.map(str::to_string),
            classification: classification.into(),
            syntax_ordinal: ordinal,
            parameter_ordinal: None,
        }
    }

    fn resolved_argument(
        source: &str,
        text: &str,
        line: u32,
        name: Option<&str>,
        classification: &str,
        syntax_ordinal: u32,
        parameter_ordinal: u32,
    ) -> engram_index::vb_extractor::VbInvocationArgument {
        let mut argument = argument(
            source,
            text,
            line,
            name,
            classification,
            syntax_ordinal,
        );
        argument.parameter_ordinal = Some(parameter_ordinal);
        argument
    }

    fn report(
        source: &str,
        calls: Vec<(&str, u32, u32, Vec<engram_index::vb_extractor::VbInvocationArgument>)>,
    ) -> InvocationReport {
        InvocationReport {
            version: "vb-invocations-v1".into(),
            request_id: None,
            source_sha256: String::new(),
            source_count: 1,
            scope: "full_source".into(),
            parse_status: "complete".into(),
            parse_error_count: 0,
            invocations: calls.into_iter().map(|(callee, start_line, end_line, arguments)| {
                let (callee_span_start, callee_span_length) = span(source, callee);
                engram_index::vb_extractor::VbInvocation {
                    source_index: 0,
                    span_start: callee_span_start,
                    span_length: callee_span_length,
                    callee_span_start,
                    callee_span_length,
                    start_line,
                    end_line,
                    target_method_id: None,
                    resolution: "lexical".into(),
                    arguments,
                }
            }).collect(),
            truncated: false,
            omitted_invocation_count: 0,
            omitted_argument_count: 0,
            notes: Vec::new(),
        }
    }

    #[test]
    fn canonical_sweep_no_added_diff_is_silent_and_does_not_query_roslyn() {
        let temp = tempfile::tempdir().unwrap();
        let sweep = canonical_call_migration_sweep_with(
            temp.path(),
            "",
            &["Changed.vb".into()],
            &[],
            CANONICAL_SWEEP_CAPS,
            |_, _| panic!("Roslyn must not run without relevant added lines"),
        );
        assert!(!sweep.attempted);
        assert!(sweep.notes.is_empty());
    }

    #[test]
    fn canonical_sweep_path_keys_follow_platform_case_semantics() {
        if cfg!(windows) {
            assert_eq!(canonical_path_key("Folder/Foo.vb"), canonical_path_key("folder/foo.vb"));
        } else {
            assert_ne!(canonical_path_key("Folder/Foo.vb"), canonical_path_key("folder/foo.vb"));
        }
    }

    #[test]
    fn canonical_sweep_scans_requested_file_before_global_file_cap() {
        let temp = tempfile::tempdir().unwrap();
        let changed = "AuditTrail.Record(\"same\")\nAuditTrail.Record(EventToken.Widget)\n";
        std::fs::write(temp.path().join("Changed.vb"), changed).unwrap();
        std::fs::write(temp.path().join("A-first.vb"), "AuditTrail.Record(\"other\")\n").unwrap();
        let diff = added_diff("Changed.vb", &[(2, "AuditTrail.Record(EventToken.Widget)")]);
        let caps = CanonicalSweepCaps { files: 1, ..CANONICAL_SWEEP_CAPS };
        let sweep = canonical_call_migration_sweep_with(
            temp.path(),
            "",
            &["Changed.vb".into()],
            &[diff],
            caps,
            |_, source| Ok(report(source, vec![
                ("AuditTrail.Record", 1, 1, vec![argument(source, "\"same\"", 1, None, "string_literal", 0)]),
                ("AuditTrail.Record", 2, 2, vec![argument(source, "EventToken.Widget", 2, None, "member_access", 0)]),
            ])),
        );
        assert_eq!(sweep.residuals.len(), 1, "{sweep:#?}");
        assert!(sweep.residuals[0].contains("Changed.vb:1"));
        assert!(!sweep.notes.iter().any(|note| note.contains("omitted requested seed")));
    }

    #[test]
    fn canonical_sweep_named_argument_matches_after_reorder_and_in_changed_file() {
        let temp = tempfile::tempdir().unwrap();
        let changed = "AuditTrail.Record(category := \"same\")\nAuditTrail.Record(\n value := 7,\n category := EventPrefix.Widget)\n";
        let legacy = "AuditTrail.Record(value := 8, category := \"other\")\n";
        std::fs::write(temp.path().join("Changed.vb"), changed).unwrap();
        std::fs::write(temp.path().join("Legacy.vb"), legacy).unwrap();
        let diff = added_diff("Changed.vb", &[(4, " category := EventPrefix.Widget)")]);
        let sweep = canonical_call_migration_sweep_with(
            temp.path(),
            "",
            &["Changed.vb".into()],
            &[diff],
            CANONICAL_SWEEP_CAPS,
            |_, source| {
                if source == changed {
                    Ok(report(source, vec![
                        ("AuditTrail.Record", 1, 1, vec![argument(source, "\"same\"", 1, Some("category"), "string_literal", 0)]),
                        ("AuditTrail.Record", 2, 4, vec![
                            argument(source, "7", 3, Some("value"), "other", 0),
                            argument(source, "EventPrefix.Widget", 4, Some("category"), "member_access", 1),
                        ]),
                    ]))
                } else {
                    Ok(report(source, vec![("AuditTrail.Record", 1, 1, vec![
                        argument(source, "\"other\"", 1, Some("category"), "string_literal", 1),
                    ])]))
                }
            },
        );
        assert!(sweep.attempted);
        assert_eq!(sweep.residuals.len(), 2, "{sweep:#?}");
        let joined = sweep.residuals.join("\n");
        assert!(joined.contains("Changed.vb:1"), "{joined}");
        assert!(joined.contains("Legacy.vb:1"), "{joined}");
        assert!(joined.contains("named argument `category`"), "{joined}");
        assert!(joined.contains("EventPrefix.Widget"), "{joined}");
    }

    #[test]
    fn canonical_sweep_resolved_parameter_matches_named_and_positional_forms() {
        let temp = tempfile::tempdir().unwrap();
        let changed = "AuditTrail.Record(category := EventToken.Widget)\n";
        let readers = "AuditTrail.Record(\"positional\")\nAuditTrail.Record(category := \"named\")\n";
        std::fs::write(temp.path().join("Changed.vb"), changed).unwrap();
        std::fs::write(temp.path().join("Readers.vb"), readers).unwrap();
        let diff = added_diff("Changed.vb", &[(1, changed.trim_end())]);
        let sweep = canonical_call_migration_sweep_with(
            temp.path(),
            "",
            &["Changed.vb".into()],
            &[diff],
            CANONICAL_SWEEP_CAPS,
            |_, source| {
                if source == changed {
                    Ok(report(source, vec![("AuditTrail.Record", 1, 1, vec![
                        resolved_argument(
                            source,
                            "EventToken.Widget",
                            1,
                            Some("category"),
                            "member_access",
                            0,
                            1,
                        ),
                    ])]))
                } else {
                    Ok(report(source, vec![
                        ("AuditTrail.Record", 1, 1, vec![resolved_argument(
                            source, "\"positional\"", 1, None, "string_literal", 0, 1,
                        )]),
                        ("AuditTrail.Record", 2, 2, vec![resolved_argument(
                            source, "\"named\"", 2, Some("category"), "string_literal", 0, 1,
                        )]),
                    ]))
                }
            },
        );
        assert_eq!(sweep.residuals.len(), 2, "{sweep:#?}");
        assert!(sweep.residuals.iter().all(|item| item.contains("resolved parameter ordinal 2")));
    }

    #[test]
    fn canonical_sweep_reports_partial_roslyn_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let source = "AuditTrail.Record(EventToken.Widget)\n";
        std::fs::write(temp.path().join("Changed.vb"), source).unwrap();
        let diff = added_diff("Changed.vb", &[(1, source.trim_end())]);
        let sweep = canonical_call_migration_sweep_with(
            temp.path(),
            "",
            &["Changed.vb".into()],
            &[diff],
            CANONICAL_SWEEP_CAPS,
            |_, source| {
                let mut result = report(source, vec![("AuditTrail.Record", 1, 1, vec![
                    argument(source, "EventToken.Widget", 1, None, "member_access", 0),
                ])]);
                result.parse_status = "syntax_errors".into();
                result.parse_error_count = 1;
                result.truncated = true;
                result.omitted_argument_count = 2;
                Ok(result)
            },
        );
        assert!(sweep.attempted);
        assert!(sweep.notes.iter().any(|note| note.contains("1 syntax error")));
        assert!(sweep.notes.iter().any(|note| note.contains("2 argument(s) omitted")));
    }

    #[test]
    fn canonical_sweep_non_utf8_bytes_count_toward_total_cap() {
        let temp = tempfile::tempdir().unwrap();
        let changed = "AuditTrail.Record(EventToken.Widget)\n";
        std::fs::write(temp.path().join("Changed.vb"), changed).unwrap();
        std::fs::write(temp.path().join("A-invalid.vb"), [0xff, 0xfe]).unwrap();
        let diff = added_diff("Changed.vb", &[(1, changed.trim_end())]);
        let caps = CanonicalSweepCaps {
            total_bytes: changed.len() as u64 + 2,
            ..CANONICAL_SWEEP_CAPS
        };
        let sweep = canonical_call_migration_sweep_with(
            temp.path(),
            "",
            &["Changed.vb".into()],
            &[diff],
            caps,
            |_, source| Ok(report(source, vec![("AuditTrail.Record", 1, 1, vec![
                argument(source, "EventToken.Widget", 1, None, "member_access", 0),
            ])])),
        );
        assert!(sweep.attempted);
        assert!(sweep.notes.iter().any(|note| note.contains("non-UTF-8")), "{sweep:#?}");
        assert!(
            sweep
                .summary
                .contains(&format!("{} source byte(s)", caps.total_bytes)),
            "{sweep:#?}"
        );
    }

    #[test]
    fn noninteractive_source_does_not_get_browser_scenarios() {
        assert!(runtime_risk_axes("Math.cs", "public int Add(int a, int b) => a + b;").is_empty());
    }

    #[tokio::test]
    async fn proposed_cases_require_matching_source_hash_and_exact_file_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let source = "Class Rules\nPublic Function Save(value As Integer) As String\nIf value < 0 Then Return \"invalid\"\nReturn \"ok\"\nEnd Function\nEnd Class\n";
        std::fs::write(tmp.path().join("Rules.vb"), source).unwrap();
        let method =
            crate::services::business_logic_service::extract_logic_methods(source, "vb").remove(0);
        let body_hash = ContentHash::compute(method.body.as_bytes()).0;
        let content = format!(
            "# Rules.Save\n**Analysis method hash**: `{body_hash}`\n## Source checks requiring review\n- Rule 1: cited source line is a comment; inspect it.\n## Business Rules\n- Negative quantities return a validation error.\n_Source: Rules.vb_\n"
        );
        let cfg = Config {
            embedding_backend: "fts_only".into(),
            ..Default::default()
        };
        let search =
            HybridSearchEngine::new(tmp.path().join("fts"), tmp.path().join("vectors"), &cfg)
                .await
                .unwrap();
        let mut docs = Vec::new();
        for (index, content) in [
            content.clone(),
            content.replace("_Source: Rules.vb_", "_Source: Other.vb_"),
        ]
        .into_iter()
        .enumerate()
        {
            docs.push(IndexDoc {
                generation: 0,
                chunk_id: index as u64,
                path: RelPath::new(&format!("__business_logic/Rules.vb/Save{index}.md")),
                language: "markdown".into(),
                content_hash: ContentHash::compute(content.as_bytes()).0,
                content,
                namespace: "business_logic".into(),
                author: None,
                timestamp: None,
                start_line: 0,
                end_line: 0,
                doc_id: format!("rule{index}"),
            });
        }
        search
            .index_docs("test", &docs, &tokio_util::sync::CancellationToken::new())
            .await
            .unwrap();
        let (cases, notes) =
            collect(&search, "test", 1, &["Rules.vb".into()], tmp.path(), 40).unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(
            cases[0].source_warnings,
            ["Rule 1: cited source line is a comment; inspect it."]
        );
        assert_eq!(
            cases[0].requirement,
            "Negative quantities return a validation error."
        );
        assert!(notes.iter().any(|note| note.contains("file scope")));
        std::fs::write(
            tmp.path().join("Rules.vb"),
            source.replace("value < 0", "value > 0"),
        )
        .unwrap();
        let (cases, notes) =
            collect(&search, "test", 1, &["Rules.vb".into()], tmp.path(), 40).unwrap();
        assert!(cases.is_empty());
        assert!(notes.iter().any(|note| note.contains("STALE:")));
        assert!(notes
            .iter()
            .any(|note| note.contains("cited source line is a comment")));
    }
}
