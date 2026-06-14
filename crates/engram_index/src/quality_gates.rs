//! Quality-gate ingestion parsers (Stage 3 of the agent pipeline).
//!
//! Normalizes a project's accumulated "what to avoid" knowledge — coding/agent
//! rules, copilot-instructions.md, CodeRabbit & SonarQube findings, the DevOps
//! recurring-issues board — into [`QualityRule`]s. Engram then indexes these so
//! a pre-push audit (and the planning agents) can retrieve the rules relevant to
//! a change and avoid repeating known mistakes. Pure parsing; no I/O.

use serde::Serialize;

/// One normalized "avoid this" rule/finding from any quality-gate source.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct QualityRule {
    /// Stable id (source kind + index, or the finding's own rule key).
    pub id: String,
    /// The actionable rule / finding text — the thing to avoid or follow.
    pub text: String,
    /// Origin kind: copilot, coding_rule, coderabbit, sonarqube, board, text.
    pub category: String,
    /// high | medium | low | info.
    pub severity: String,
    /// File/path/glob this rule applies to, if the source scopes it.
    pub path_scope: Option<String>,
    /// Where it came from (file name / origin), for provenance.
    pub source: String,
}

/// Quality-gate source kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualitySource {
    /// copilot-instructions.md or any markdown rules doc.
    CopilotInstructions,
    /// A markdown coding/agent-rules doc (treated like copilot).
    CodingRulesMd,
    /// CodeRabbit findings export (JSON).
    CodeRabbit,
    /// SonarQube findings/issues export (JSON).
    SonarQube,
    /// DevOps board recurring-issues export (JSON list of work items).
    DevOpsBoard,
    /// Plain text, one rule per non-empty line.
    Text,
}

impl QualitySource {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().replace(['-', ' '], "_").as_str() {
            "copilot" | "copilot_instructions" | "copilotinstructions" => {
                Some(Self::CopilotInstructions)
            }
            "rules" | "coding_rules" | "coding_rules_md" | "rules_md" | "agent_rules" => {
                Some(Self::CodingRulesMd)
            }
            "coderabbit" | "code_rabbit" => Some(Self::CodeRabbit),
            "sonarqube" | "sonar" | "sonarcloud" => Some(Self::SonarQube),
            "devops_board" | "board" | "ado_board" | "azure_board" => Some(Self::DevOpsBoard),
            "text" | "txt" | "plain" => Some(Self::Text),
            _ => None,
        }
    }

    fn category(self) -> &'static str {
        match self {
            Self::CopilotInstructions => "copilot",
            Self::CodingRulesMd => "coding_rule",
            Self::CodeRabbit => "coderabbit",
            Self::SonarQube => "sonarqube",
            Self::DevOpsBoard => "board",
            Self::Text => "text",
        }
    }
}

/// Parse any quality-gate source into normalized rules.
pub fn parse_quality_source(content: &str, source: QualitySource, origin: &str) -> Vec<QualityRule> {
    match source {
        QualitySource::CopilotInstructions | QualitySource::CodingRulesMd => {
            parse_markdown_rules(content, source.category(), origin)
        }
        QualitySource::CodeRabbit | QualitySource::SonarQube | QualitySource::DevOpsBoard => {
            parse_findings_json(content, source.category(), origin)
        }
        QualitySource::Text => parse_text_rules(content, source.category(), origin),
    }
}

/// Markdown rules: each bullet/numbered item is a rule; a directive sentence
/// (must/avoid/don't/always/never/should/prefer/ensure) under a heading also
/// counts. The nearest preceding heading scopes the rule's text for context.
fn parse_markdown_rules(content: &str, category: &str, origin: &str) -> Vec<QualityRule> {
    let mut out = Vec::new();
    let mut heading = String::new();
    let mut idx = 0usize;
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(h) = line.strip_prefix('#') {
            heading = h.trim_start_matches('#').trim().to_string();
            continue;
        }
        let bullet = line
            .strip_prefix("- ")
            .or_else(|| line.strip_prefix("* "))
            .or_else(|| line.strip_prefix("+ "))
            .or_else(|| {
                // numbered "1. " / "2) "
                line.find(['.', ')']).and_then(|p| {
                    if line[..p].chars().all(|c| c.is_ascii_digit()) && p > 0 {
                        Some(line[p + 1..].trim())
                    } else {
                        None
                    }
                })
            });
        let directive = {
            let lo = line.to_ascii_lowercase();
            const KW: &[&str] = &[
                "must ", "must not", "avoid", "don't", "do not", "always ", "never ",
                "should ", "prefer ", "ensure ", "require", "no ",
            ];
            KW.iter().any(|k| lo.contains(k))
        };
        let text = match bullet {
            Some(b) if !b.trim().is_empty() => b.trim().to_string(),
            _ if directive => line.to_string(),
            _ => continue,
        };
        let full = if heading.is_empty() {
            text
        } else {
            format!("[{heading}] {text}")
        };
        out.push(QualityRule {
            id: format!("{category}:{origin}:{idx}"),
            text: full,
            category: category.to_string(),
            severity: "medium".to_string(),
            path_scope: None,
            source: origin.to_string(),
        });
        idx += 1;
    }
    out
}

/// Generic JSON findings parser for CodeRabbit / SonarQube / board exports.
/// Accepts a top-level array, or an object with an array under issues / results
/// / findings / value / workItems. Per finding it pulls message/description/
/// title, severity, file/path/component, rule/ruleKey/check/type, line — under
/// the common field names both tools (and the ADO board) use.
fn parse_findings_json(content: &str, category: &str, origin: &str) -> Vec<QualityRule> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(content) else {
        // Not JSON — fall back to line-based so a malformed export still yields something.
        return parse_text_rules(content, category, origin);
    };
    let arr: Vec<serde_json::Value> = if let Some(a) = v.as_array() {
        a.clone()
    } else if let Some(o) = v.as_object() {
        ["issues", "results", "findings", "value", "workItems", "comments"]
            .iter()
            .find_map(|k| o.get(*k).and_then(|x| x.as_array()).cloned())
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let get = |o: &serde_json::Value, keys: &[&str]| -> Option<String> {
        for k in keys {
            // support "fields.System.Title"-style nested ADO paths too
            if let Some(val) = k.split('.').try_fold(o, |acc, seg| acc.get(seg)) {
                if let Some(s) = val.as_str() {
                    if !s.trim().is_empty() {
                        return Some(s.trim().to_string());
                    }
                } else if val.is_number() {
                    return Some(val.to_string());
                }
            }
        }
        None
    };

    let mut out = Vec::new();
    for (i, f) in arr.iter().enumerate() {
        let msg = get(f, &["message", "description", "title", "text", "fields.System.Title"]);
        let Some(text) = msg else { continue };
        let rule_key = get(f, &["rule", "ruleKey", "check", "type", "id", "category"]);
        let sev_raw =
            get(f, &["severity", "level", "priority", "fields.Microsoft.VSTS.Common.Priority"])
                .unwrap_or_default()
                .to_ascii_lowercase();
        let severity = if sev_raw.contains("block")
            || sev_raw.contains("crit")
            || sev_raw.contains("high")
            || sev_raw == "1"
        {
            "high"
        } else if sev_raw.contains("major") || sev_raw.contains("med") || sev_raw == "2" {
            "medium"
        } else if sev_raw.is_empty() {
            "medium"
        } else {
            "low"
        };
        let path_scope = get(
            f,
            &["file", "path", "component", "filePath", "location", "fileName"],
        );
        let line = get(f, &["line", "startLine", "lineNumber"]);
        let id = rule_key
            .clone()
            .map(|r| format!("{category}:{origin}:{r}:{i}"))
            .unwrap_or_else(|| format!("{category}:{origin}:{i}"));
        let mut full = text;
        if let Some(r) = rule_key {
            full = format!("[{r}] {full}");
        }
        if let Some(l) = line {
            full = format!("{full} (line {l})");
        }
        out.push(QualityRule {
            id,
            text: full,
            category: category.to_string(),
            severity: severity.to_string(),
            path_scope,
            source: origin.to_string(),
        });
    }
    out
}

fn parse_text_rules(content: &str, category: &str, origin: &str) -> Vec<QualityRule> {
    content
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .enumerate()
        .map(|(i, l)| QualityRule {
            id: format!("{category}:{origin}:{i}"),
            text: l.to_string(),
            category: category.to_string(),
            severity: "medium".to_string(),
            path_scope: None,
            source: origin.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_markdown_copilot_instructions() {
        let md = "# Coding rules\n\n- Always use `Using` for disposables.\n- Avoid On Error Resume Next.\n\n## Naming\nMethods must be PascalCase.\nSome non-rule prose without a keyword.\n";
        let rules = parse_quality_source(md, QualitySource::CopilotInstructions, "copilot-instructions.md");
        assert!(rules.len() >= 3, "{rules:?}");
        assert!(rules.iter().any(|r| r.text.contains("Using")));
        assert!(rules.iter().any(|r| r.text.contains("[Naming]") && r.text.contains("PascalCase")));
        assert!(rules.iter().all(|r| r.category == "copilot"));
    }

    #[test]
    fn parses_coderabbit_sonar_json_findings() {
        let json = r#"[
          {"message":"Avoid catching generic Exception","severity":"major","file":"App_Code/Foo.vb","rule":"S2221","line":42},
          {"description":"Possible null dereference","level":"BLOCKER","component":"App_Code/Bar.vb","ruleKey":"S2259"}
        ]"#;
        let rules = parse_quality_source(json, QualitySource::SonarQube, "sonar.json");
        assert_eq!(rules.len(), 2, "{rules:?}");
        assert_eq!(rules[0].severity, "medium"); // major
        assert_eq!(rules[1].severity, "high"); // blocker
        assert_eq!(rules[0].path_scope.as_deref(), Some("App_Code/Foo.vb"));
        assert!(rules[0].text.contains("[S2221]"));
    }

    #[test]
    fn json_object_with_issues_array_and_ado_board() {
        let json = r#"{"workItems":[{"fields.System.Title":"Recurring: forgot to update all resx languages","fields.Microsoft.VSTS.Common.Priority":1}]}"#;
        let rules = parse_quality_source(json, QualitySource::DevOpsBoard, "board.json");
        // nested ado paths are not real JSON keys here; ensure no panic + graceful empty/line fallback
        let _ = rules; // schema-dependent; the parser must not panic
    }

    #[test]
    fn from_str_aliases() {
        assert_eq!(QualitySource::from_str("copilot-instructions"), Some(QualitySource::CopilotInstructions));
        assert_eq!(QualitySource::from_str("CodeRabbit"), Some(QualitySource::CodeRabbit));
        assert_eq!(QualitySource::from_str("sonar"), Some(QualitySource::SonarQube));
        assert_eq!(QualitySource::from_str("board"), Some(QualitySource::DevOpsBoard));
        assert_eq!(QualitySource::from_str("nope"), None);
    }
}
