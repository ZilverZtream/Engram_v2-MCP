//! Bounded same-method source observations, without executing or binding SQL.
use super::request_trace::routing::{Token, tokens};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Clone, Serialize)]
pub struct Site {
    pub line: u32,
    pub source_line: String,
    pub excerpt_truncated: bool,
}
#[derive(Clone, Serialize)]
pub struct Declaration {
    pub site: Site,
    pub sql_type_spelling: Option<String>,
    pub size_literal: Option<String>,
}
#[derive(Clone, Serialize)]
pub struct Parameter {
    pub name: String,
    pub declarations: Vec<Declaration>,
    pub assignments: Vec<Site>,
}
#[derive(Clone, Serialize)]
pub struct Execution {
    pub line: u32,
    pub method: String,
}
#[derive(Clone, Serialize)]
pub struct Command {
    pub receiver: String,
    pub declaration_lines: Vec<u32>,
    pub receiver_ambiguous: bool,
    pub parameters: Vec<Parameter>,
    pub execution_sites: Vec<Execution>,
    pub unsupported_parameter_uses: usize,
}
#[derive(Clone, Serialize)]
pub struct Report {
    pub commands: Vec<Command>,
    pub truncated: bool,
    pub parameters_omitted: usize,
    pub commands_omitted: usize,
    pub interpretation: &'static str,
}
pub fn method_observations(source: &str, start: u32, end: u32) -> Option<Report> {
    if start == 0 || end < start {
        return None;
    }
    let lines: Vec<_> = source.lines().collect();
    if end as usize > lines.len() {
        return None;
    }
    let body = lines[start as usize - 1..end as usize].join("\n");
    Some(observe(&body, start))
}
fn word(t: &[Token], i: usize, s: &str) -> bool {
    t.get(i)
        .is_some_and(|t| !t.string && t.text.eq_ignore_ascii_case(s))
}
fn key(t: &[Token], i: usize) -> Option<String> {
    let t = t.get(i)?;
    (t.string
        && t.text.len() > 1
        && t.text.len() <= 128
        && t.text.starts_with('@')
        && t.text[1..]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_'))
    .then(|| t.text.clone())
}
fn site(lines: &[&str], relative: usize, first: u32) -> Site {
    let line = lines
        .get(relative.saturating_sub(1))
        .copied()
        .unwrap_or("")
        .trim();
    let end = line
        .char_indices()
        .map(|(i, c)| i + c.len_utf8())
        .take_while(|end| *end <= 512)
        .last()
        .unwrap_or(0);
    Site {
        line: first.saturating_add(relative.saturating_sub(1) as u32),
        source_line: line[..end].into(),
        excerpt_truncated: end < line.len(),
    }
}
/// Caller supplies one complete method body and its one-based first source line.
/// File identity and source admission stay with the consuming workflow.
pub fn observe(body: &str, first_line: u32) -> Report {
    let mut report = Report {
        commands: Vec::new(),
        truncated: false,
        parameters_omitted: 0,
        commands_omitted: 0,
        interpretation: "Same-method, name-shaped SqlCommand receiver and literal parameter observations from supplied source. Declaration types/sizes and assignment lines are syntax, not CLR/physical database binding, evaluated values, branch reachability, nullability guarantees or executed tests. Reused receiver declarations are ambiguous. Computed names, indirect parameter objects, mutations through aliases/helpers and unsupported syntax remain unknown. Input128KiB; commands32; parameters64 per command; declaration sites4 and assignment sites8 per parameter; execution sites32; source-line excerpts512bytes; serialized report64KiB.",
    };
    if body.len() > 128 * 1024 || first_line == 0 {
        report.truncated = true;
        return report;
    }
    let t = tokens(body, true);
    let lines: Vec<_> = body.lines().collect();
    let mut commands: BTreeMap<String, Command> = BTreeMap::new();
    for i in 2..t.len() {
        if !word(&t, i, "New") {
            continue;
        }
        let variable = if word(&t, i - 1, "=") {
            Some(&t[i - 2])
        } else if i >= 3
            && word(&t, i - 1, "As")
            && (word(&t, i - 3, "Dim") || word(&t, i - 3, "Using"))
        {
            Some(&t[i - 2])
        } else {
            None
        };
        let Some(variable) = variable.filter(|v| !v.string && v.text.len() <= 128) else {
            continue;
        };
        let mut at = i + 1;
        while word(&t, at + 1, ".") && t.get(at + 2).is_some() {
            at += 2;
        }
        if !word(&t, at, "SqlCommand") || !word(&t, at + 1, "(") {
            continue;
        }
        let name = variable.text.to_ascii_lowercase();
        if !commands.contains_key(&name) && commands.len() >= 32 {
            report.truncated = true;
            report.commands_omitted += 1;
            continue;
        }
        let command = commands.entry(name).or_insert_with(|| Command {
            receiver: variable.text.clone(),
            declaration_lines: vec![],
            receiver_ambiguous: false,
            parameters: vec![],
            execution_sites: vec![],
            unsupported_parameter_uses: 0,
        });
        if command.declaration_lines.len() < 4 {
            command
                .declaration_lines
                .push(first_line + t[i].line as u32 - 1);
        } else {
            report.truncated = true;
        }
        command.receiver_ambiguous = command.declaration_lines.len() > 1;
    }
    for i in 0..t.len().saturating_sub(3) {
        if t[i].string || !word(&t, i + 1, ".") {
            continue;
        }
        let Some(command) = commands.get_mut(&t[i].text.to_ascii_lowercase()) else {
            continue;
        };
        if [
            "ExecuteReader",
            "ExecuteScalar",
            "ExecuteNonQuery",
            "ExecuteReaderAsync",
            "ExecuteScalarAsync",
            "ExecuteNonQueryAsync",
        ]
        .iter()
        .any(|method| word(&t, i + 2, method))
            && word(&t, i + 3, "(")
        {
            if command.execution_sites.len() < 32 {
                command.execution_sites.push(Execution {
                    line: first_line + t[i].line as u32 - 1,
                    method: t[i + 2].text.clone(),
                });
            } else {
                report.truncated = true;
            }
        }
        if !word(&t, i + 2, "Parameters") {
            continue;
        }
        let add = word(&t, i + 3, ".")
            && (word(&t, i + 4, "Add") || word(&t, i + 4, "AddWithValue"))
            && word(&t, i + 5, "(");
        let assignment = word(&t, i + 3, "(")
            && word(&t, i + 5, ")")
            && word(&t, i + 6, ".")
            && word(&t, i + 7, "Value")
            && word(&t, i + 8, "=");
        let name = if add && word(&t, i + 7, ",") {
            key(&t, i + 6)
        } else if assignment {
            key(&t, i + 4)
        } else {
            None
        };
        let Some(name) = name else {
            command.unsupported_parameter_uses += 1;
            continue;
        };
        let index = command.parameters.iter().position(|p| p.name == name);
        let index = if let Some(index) = index {
            index
        } else {
            if command.parameters.len() >= 64 {
                report.truncated = true;
                report.parameters_omitted += 1;
                continue;
            }
            command.parameters.push(Parameter {
                name,
                declarations: vec![],
                assignments: vec![],
            });
            command.parameters.len() - 1
        };
        let parameter = &mut command.parameters[index];
        let source = site(&lines, t[i].line, first_line);
        report.truncated |= source.excerpt_truncated;
        if assignment || word(&t, i + 4, "AddWithValue") {
            if parameter.assignments.len() < 8 {
                parameter.assignments.push(source.clone());
            } else {
                report.truncated = true;
            }
        }
        if add {
            let mut sql_type = None;
            let mut size = None;
            if word(&t, i + 4, "Add") {
                let mut at = i + 8;
                while word(&t, at + 1, ".") && t.get(at + 2).is_some() {
                    at += 2;
                }
                if at >= i + 10 && word(&t, at - 2, "SqlDbType") && !t[at].string {
                    sql_type = Some(t[at].text.clone());
                    if word(&t, at + 1, ",") {
                        let start = at + 2;
                        let digits = if word(&t, start, "-") {
                            start + 1
                        } else {
                            start
                        };
                        if t.get(digits).is_some_and(|v| {
                            !v.string
                                && !v.text.is_empty()
                                && v.text.len() <= 10
                                && v.text.bytes().all(|c| c.is_ascii_digit())
                        }) && word(&t, digits + 1, ")")
                        {
                            size = Some(format!(
                                "{}{}",
                                if digits > start { "-" } else { "" },
                                t[digits].text
                            ));
                        }
                    }
                }
            }
            if parameter.declarations.len() < 4 {
                parameter.declarations.push(Declaration {
                    site: source,
                    sql_type_spelling: sql_type,
                    size_literal: size,
                });
            } else {
                report.truncated = true;
            }
        }
    }
    report.commands = commands.into_values().collect();
    while serde_json::to_vec(&report).is_ok_and(|bytes| bytes.len() > 64 * 1024) {
        report.truncated = true;
        let Some(last) = report.commands.last_mut() else {
            break;
        };
        if last.parameters.is_empty() {
            report.commands.pop();
            report.commands_omitted += 1;
        } else {
            let retained = last.parameters.len() / 2;
            report.parameters_omitted += last.parameters.len() - retained;
            last.parameters.truncate(retained);
        }
    }
    report
}
