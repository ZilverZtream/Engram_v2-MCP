//! Hot-loaded planning contract rules.
//!
//! Organization rules live at `data_dir/rules/planning-contract-rules.yaml`;
//! repository rules live at `.engram/planning-contract-rules.yaml` and
//! override organization rules by stable id. The files are read for each
//! `get_change_set` call, allowing planning policy to improve without a server
//! rebuild or restart. Rules are declarative substring predicates, bounded in
//! size and count, and provenance-filtered during historical replay.

use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashSet},
    path::Path,
};

const RULE_PACK_MAX_BYTES: u64 = 256 * 1024;
const RULE_PACK_MAX_RULES: usize = 128;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanningRuleFile {
    version: u32,
    #[serde(default)]
    introduced_at: Option<String>,
    #[serde(default)]
    provenance: Option<String>,
    #[serde(default)]
    rules: Vec<ConfiguredPlanningRule>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfiguredPlanningRule {
    id: String,
    title: String,
    requirement: String,
    #[serde(default = "default_severity")]
    severity: String,
    #[serde(default)]
    story_all: Vec<String>,
    #[serde(default)]
    story_any: Vec<String>,
    #[serde(default)]
    story_none: Vec<String>,
    #[serde(default)]
    path_any: Vec<String>,
    #[serde(default)]
    oracle_guard: Option<String>,
    #[serde(default)]
    introduced_at: Option<String>,
    #[serde(default)]
    provenance: Option<String>,
    #[serde(skip)]
    source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanningContractRuleMatch {
    pub id: String,
    pub title: String,
    pub requirement: String,
    pub severity: String,
    pub oracle_guard: Option<String>,
    pub source: String,
    pub introduced_at: Option<String>,
    pub provenance: Option<String>,
}

fn default_severity() -> String {
    "required_if_applicable".into()
}

fn clean(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}

fn valid_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
        && value[5..7]
            .parse::<u8>()
            .is_ok_and(|month| (1..=12).contains(&month))
        && value[8..10]
            .parse::<u8>()
            .is_ok_and(|day| (1..=31).contains(&day))
}

fn normalize_rule(
    mut rule: ConfiguredPlanningRule,
    source: &str,
) -> Result<ConfiguredPlanningRule, String> {
    if !clean(&rule.id, 64)
        || !rule
            .id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Err(
            "planning rule id must be 1-64 ASCII letters, digits, dot, dash or underscore".into(),
        );
    }
    if !clean(&rule.title, 160) || !clean(&rule.requirement, 1_500) {
        return Err(format!(
            "planning rule {} title/requirement is invalid",
            rule.id
        ));
    }
    rule.severity = rule.severity.trim().to_ascii_lowercase();
    if !matches!(
        rule.severity.as_str(),
        "release_blocking_if_applicable" | "required_if_applicable" | "advisory"
    ) {
        return Err(format!(
            "planning rule {} severity must be release_blocking_if_applicable, required_if_applicable or advisory",
            rule.id
        ));
    }
    let selectors = [
        &rule.story_all,
        &rule.story_any,
        &rule.story_none,
        &rule.path_any,
    ];
    if selectors.iter().any(|values| values.len() > 32)
        || selectors
            .iter()
            .flat_map(|values| values.iter())
            .any(|value| !clean(value, 128))
    {
        return Err(format!(
            "planning rule {} has too many or invalid predicates",
            rule.id
        ));
    }
    if rule.story_all.is_empty() && rule.story_any.is_empty() && rule.path_any.is_empty() {
        return Err(format!(
            "planning rule {} needs a positive predicate",
            rule.id
        ));
    }
    if rule
        .oracle_guard
        .as_deref()
        .is_some_and(|value| !clean(value, 1_500))
    {
        return Err(format!("planning rule {} oracle_guard is invalid", rule.id));
    }
    if rule
        .introduced_at
        .as_deref()
        .is_some_and(|date| !valid_date(date))
    {
        return Err(format!(
            "planning rule {} introduced_at must be YYYY-MM-DD",
            rule.id
        ));
    }
    if rule
        .provenance
        .as_deref()
        .is_some_and(|value| !clean(value, 300))
    {
        return Err(format!("planning rule {} provenance is invalid", rule.id));
    }
    rule.id = rule.id.trim().to_ascii_lowercase();
    rule.title = rule.title.trim().to_string();
    rule.requirement = rule.requirement.trim().to_string();
    rule.oracle_guard = rule.oracle_guard.map(|value| value.trim().to_string());
    for values in [
        &mut rule.story_all,
        &mut rule.story_any,
        &mut rule.story_none,
        &mut rule.path_any,
    ] {
        for value in values.iter_mut() {
            *value = value.trim().replace('\\', "/").to_ascii_lowercase();
        }
    }
    rule.source = source.to_string();
    Ok(rule)
}

fn read_rule_file(
    path: &Path,
    boundary: &Path,
    source: &str,
) -> Result<Vec<ConfiguredPlanningRule>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let boundary = boundary
        .canonicalize()
        .map_err(|error| format!("{source} planning-rule boundary unavailable: {error}"))?;
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("{source} planning rules unavailable: {error}"))?;
    if !canonical.starts_with(&boundary) {
        return Err(format!(
            "{source} planning rules resolve outside their configured boundary"
        ));
    }
    let metadata = std::fs::metadata(&canonical)
        .map_err(|error| format!("{source} planning-rule metadata unavailable: {error}"))?;
    if !metadata.is_file() || metadata.len() > RULE_PACK_MAX_BYTES {
        return Err(format!(
            "{source} planning rules must be a file no larger than {RULE_PACK_MAX_BYTES} bytes"
        ));
    }
    let bytes = std::fs::read(&canonical)
        .map_err(|error| format!("{source} planning rules cannot be read: {error}"))?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| format!("{source} planning rules must be UTF-8"))?;
    let parsed: PlanningRuleFile = serde_yaml::from_str(text)
        .map_err(|error| format!("{source} planning rules have invalid YAML/schema: {error}"))?;
    if parsed.version != 1 {
        return Err(format!("{source} planning-rule version must be 1"));
    }
    if parsed.rules.len() > RULE_PACK_MAX_RULES {
        return Err(format!(
            "{source} planning rules exceed {RULE_PACK_MAX_RULES} rules"
        ));
    }
    if parsed
        .introduced_at
        .as_deref()
        .is_some_and(|date| !valid_date(date))
    {
        return Err(format!(
            "{source} planning-rule introduced_at must be YYYY-MM-DD"
        ));
    }
    if parsed
        .provenance
        .as_deref()
        .is_some_and(|value| !clean(value, 300))
    {
        return Err(format!("{source} planning-rule provenance is invalid"));
    }
    let mut seen = HashSet::new();
    let default_date = parsed.introduced_at;
    let default_provenance = parsed.provenance;
    parsed
        .rules
        .into_iter()
        .map(|mut rule| {
            if rule.introduced_at.is_none() {
                rule.introduced_at = default_date.clone();
            }
            if rule.provenance.is_none() {
                rule.provenance = default_provenance.clone();
            }
            let rule = normalize_rule(rule, source)?;
            if !seen.insert(rule.id.clone()) {
                return Err(format!("{source} planning rules repeat id {}", rule.id));
            }
            Ok(rule)
        })
        .collect()
}

fn matches(rule: &ConfiguredPlanningRule, story: &str, paths: &[String]) -> bool {
    let lower_story = story.to_ascii_lowercase();
    let lower_paths = paths
        .iter()
        .map(|path| path.replace('\\', "/").to_ascii_lowercase())
        .collect::<Vec<_>>();
    rule.story_all.iter().all(|term| lower_story.contains(term))
        && (rule.story_any.is_empty()
            || rule.story_any.iter().any(|term| lower_story.contains(term)))
        && rule
            .story_none
            .iter()
            .all(|term| !lower_story.contains(term))
        && (rule.path_any.is_empty()
            || rule
                .path_any
                .iter()
                .any(|term| lower_paths.iter().any(|path| path.contains(term))))
}

/// Load and match both rule packs for one planning call. Project rules replace
/// organization rules with the same stable id. A historical cutoff excludes
/// undated rules and rules introduced on or after the exclusive cutoff.
pub(crate) fn load_matching_planning_rules(
    data_dir: &Path,
    project_root: &Path,
    story: &str,
    paths: &[String],
    knowledge_before: Option<&str>,
) -> (Vec<PlanningContractRuleMatch>, Vec<String>) {
    let sources = [
        (
            data_dir.join("rules/planning-contract-rules.yaml"),
            data_dir,
            "global",
        ),
        (
            project_root.join(".engram/planning-contract-rules.yaml"),
            project_root,
            "project",
        ),
    ];
    let mut merged = BTreeMap::new();
    let mut notes = Vec::new();
    for (path, boundary, source) in sources {
        match read_rule_file(&path, boundary, source) {
            Ok(rules) => {
                for rule in rules {
                    if let Some(cutoff) = knowledge_before {
                        match rule.introduced_at.as_deref() {
                            Some(date) if date < cutoff => {}
                            Some(date) => {
                                notes.push(format!("planning rule {} excluded: introduced_at {date} is not before historical cutoff {cutoff}", rule.id));
                                continue;
                            }
                            None => {
                                notes.push(format!("planning rule {} excluded: no introduced_at provenance for historical cutoff {cutoff}", rule.id));
                                continue;
                            }
                        }
                    }
                    merged.insert(rule.id.clone(), rule);
                }
            }
            Err(error) => notes.push(error),
        }
    }
    let matched = merged
        .into_values()
        .filter(|rule| matches(rule, story, paths))
        .map(|rule| PlanningContractRuleMatch {
            id: format!("RULE-{}", rule.id.to_ascii_uppercase()),
            title: rule.title,
            requirement: rule.requirement,
            severity: rule.severity,
            oracle_guard: rule.oracle_guard,
            source: rule.source,
            introduced_at: rule.introduced_at,
            provenance: rule.provenance,
        })
        .collect();
    (matched, notes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_rule_overrides_global_without_restart() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let project = temp.path().join("project");
        std::fs::create_dir_all(data.join("rules")).unwrap();
        std::fs::create_dir_all(project.join(".engram")).unwrap();
        std::fs::write(
            data.join("rules/planning-contract-rules.yaml"),
            r#"
version: 1
introduced_at: 2020-01-01
rules:
  - id: CONTINUITY
    title: global title
    requirement: global requirement
    story_any: [password]
"#,
        )
        .unwrap();
        std::fs::write(
            project.join(".engram/planning-contract-rules.yaml"),
            r#"
version: 1
introduced_at: 2020-01-02
rules:
  - id: continuity
    title: project title
    requirement: project requirement
    severity: release_blocking_if_applicable
    story_any: [password]
"#,
        )
        .unwrap();

        let (matched, notes) =
            load_matching_planning_rules(&data, &project, "change password", &[], None);
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].id, "RULE-CONTINUITY");
        assert_eq!(matched[0].requirement, "project requirement");
        assert_eq!(matched[0].severity, "release_blocking_if_applicable");

        std::fs::write(
            project.join(".engram/planning-contract-rules.yaml"),
            r#"
version: 1
introduced_at: 2020-01-02
rules:
  - id: continuity
    title: project title
    requirement: updated without rebuilding
    severity: release_blocking_if_applicable
    story_any: [password]
"#,
        )
        .unwrap();
        let (reloaded, notes) =
            load_matching_planning_rules(&data, &project, "change password", &[], None);
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(reloaded[0].requirement, "updated without rebuilding");
    }

    #[test]
    fn historical_cutoff_excludes_undated_and_future_rules() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let project = temp.path().join("project");
        std::fs::create_dir_all(data.join("rules")).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            data.join("rules/planning-contract-rules.yaml"),
            r#"
version: 1
rules:
  - id: old
    title: old
    requirement: old requirement
    introduced_at: 2020-01-01
    story_any: [session]
  - id: future
    title: future
    requirement: future requirement
    introduced_at: 2021-01-01
    story_any: [session]
  - id: undated
    title: undated
    requirement: undated requirement
    story_any: [session]
"#,
        )
        .unwrap();

        let (matched, notes) = load_matching_planning_rules(
            &data,
            &project,
            "session lifecycle",
            &[],
            Some("2020-06-01"),
        );
        assert_eq!(
            matched
                .iter()
                .map(|rule| rule.id.as_str())
                .collect::<Vec<_>>(),
            ["RULE-OLD"]
        );
        assert_eq!(notes.len(), 2);
    }

    #[test]
    fn path_and_story_predicates_must_both_match() {
        let rule = ConfiguredPlanningRule {
            id: "x".into(),
            title: "x".into(),
            requirement: "x".into(),
            severity: default_severity(),
            story_all: vec!["session".into()],
            story_any: vec![],
            story_none: vec!["public".into()],
            path_any: vec!["api/".into()],
            oracle_guard: None,
            introduced_at: None,
            provenance: None,
            source: "test".into(),
        };
        assert!(matches(
            &rule,
            "session expiry",
            &["Site/API/Auth.vb".into()]
        ));
        assert!(!matches(
            &rule,
            "public session expiry",
            &["Site/API/Auth.vb".into()]
        ));
        assert!(!matches(
            &rule,
            "session expiry",
            &["Site/Page.aspx".into()]
        ));
    }
}
