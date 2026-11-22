use super::project_policy::{self, PolicyBudget};
use std::path::Path;

pub struct Rule {
    language_scoped:bool,
    pub source: String,
    pub text: String,
    pub public_upper: Option<bool>,
}

pub fn is_typescript(path: &str) -> bool {
    matches!(path.rsplit('.').next(), Some("ts" | "tsx" | "mts" | "cts"))
}

/// Read the checkout policy for this path. A Markdown language heading
/// narrows the rule even when the instruction file applies to all languages.
pub fn for_file(root: &Path, target: &str) -> Vec<Rule> {
    if !is_typescript(target) {
        return Vec::new();
    }
    let context = project_policy::resolve_with_budget(
        root,
        &[target.to_owned()],
        &[],
        PolicyBudget {
            max_sources: 32,
            max_characters_per_source: 120_000,
            max_characters_total: 400_000,
        },
    );
    let mut rules = Vec::new();
    for source in context.sources {
        let p = &source.path;
        if !(p.ends_with("AGENTS.md")
            || p.ends_with("CLAUDE.md")
            || p.ends_with("copilot-instructions.md")
            || p.ends_with(".instructions.md"))
        {
            continue;
        }
        let mut scope = Vec::<(usize, bool)>::new();
        let mut fence = false;
        for (i, line) in source.content.lines().enumerate() {
            let s = line.trim();
            if s.starts_with("```") || s.starts_with("~~~") {
                fence = !fence;
                continue;
            }
            if fence {
                continue;
            }
            let l = s.to_ascii_lowercase().replace(['*', '`'], "");
            let depth = l.bytes().take_while(|b| *b == b'#').count();
            if depth > 0 {
                scope.retain(|(d, _)| *d < depth);
                if ["typescript", "javascript", "vb.net", "c#", "sql", "python"]
                    .iter()
                    .any(|v| l.contains(v))
                {
                    scope.push((depth, l.contains("typescript") || l.contains("javascript")));
                }
                continue;
            }
            if scope.last().is_some_and(|(_, ok)| !*ok)
                || !s.starts_with("- ")
                || s.contains("(qg-")
            {
                continue;
            }
            let Some(case) = ["pascalcase", "_camelcase", "camelcase", "snake_case"]
                .into_iter()
                .find(|c| l.starts_with(&format!("- {c} for ")))
            else {
                continue;
            };
            let public_upper = (l.contains("for public members")
                || l.contains("for public methods"))
            .then_some(case)
            .and_then(|c| match c {
                "pascalcase" => Some(true),
                "camelcase" => Some(false),
                _ => None,
            });
            rules.push(Rule {
                language_scoped:scope.last().is_some_and(|(_,applies)|*applies),
                source: format!("{p}:{}", i + 1),
                text: s.trim_start_matches("- ").to_owned(),
                public_upper,
            });
        }
    }
    rules
}

pub fn public_rule(rules: &[Rule]) -> Option<(&Rule, bool)> {
    let language_scoped=rules.iter().any(|r|r.public_upper.is_some() && r.language_scoped);
    let mut candidates = rules.iter().filter(|r|!language_scoped || r.language_scoped).filter_map(|r| Some((r, r.public_upper?)));
    let first = candidates.next()?;
    candidates
        .all(|(_, value)| value == first.1)
        .then_some(first)
}

pub fn violates(name: &str, upper: bool) -> bool {
    name.contains('_')
        || name.chars().next().is_some_and(|c| {
            if upper {
                c.is_lowercase()
            } else {
                c.is_uppercase()
            }
        })
}
