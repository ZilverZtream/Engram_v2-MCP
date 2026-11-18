//! Guidance for explicit prospective source paths, without asserting code exists.
use crate::services::project_policy::{self, PolicyContext};
use std::{collections::BTreeSet, path::Path};

fn declared_paths(question: &str) -> (BTreeSet<String>, bool) {
    let instruction_question = question
        .split_whitespace()
        .filter(|token| !token.contains(['/', '\\']))
        .flat_map(|token| token.split(|c: char| !c.is_ascii_alphabetic()))
        .any(|word| {
            matches!(
                word.to_ascii_lowercase().as_str(),
                "instruction"
                    | "instructions"
                    | "guidance"
                    | "policy"
                    | "policies"
                    | "rule"
                    | "rules"
            )
        });
    let mut paths = BTreeSet::new();
    if !instruction_question {
        return (paths, false);
    }
    for token in question.split_whitespace() {
        let token = token.trim_matches(['`', '"', '\'', '(', ')', '[', ']', ',', ';', '?', '!']);
        if token.len() > 4096 || token.contains(['*', '?', '{', '}']) {
            continue;
        }
        let Some(path) = project_policy::normalized_relative(token) else {
            continue;
        };
        let lower = path.to_ascii_lowercase();
        if ![
            ".vb", ".aspx", ".ascx", ".master", ".ashx", ".asmx", ".js", ".ts", ".sql",
        ]
        .iter()
        .any(|extension| lower.ends_with(extension))
        {
            continue;
        }
        if paths.len() == 16 && !paths.contains(&path) {
            return (paths, true);
        }
        paths.insert(path);
    }
    (paths, false)
}

pub(super) fn resolve(root: &Path, question: &str, returned_paths: &[String]) -> PolicyContext {
    let (declared, truncated) = declared_paths(question);
    let paths = returned_paths
        .iter()
        .cloned()
        .chain(declared.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut context = project_policy::resolve(root, &paths, &[]);
    context.notes.push("Policy scope covers the code paths returned in this report, not all files a future edit may touch. Recheck guidance for the final edit scope.".into());
    if !declared.is_empty() {
        context.notes.push(format!("Policy scope also includes source paths explicitly named in the instruction question: {}. These may be prospective files; their existence, implementation and compliance are not established. No source body is read for this policy selection.", declared.into_iter().collect::<Vec<_>>().join(", ")));
    }
    if truncated {
        context.notes.push("Explicit instruction target selection stopped at16 distinct source paths; further named files require a separate scoped question.".into());
    }
    context
}
