//! Source-backed observations about call sites and the first hop of execution.
use regex::Regex;
use std::{collections::BTreeMap, sync::LazyLock};

static ALLOC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bnew\s+[\w.]*context\b").unwrap());
static WRITE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\.\s*(?:create|insert\w*|delete\w*|update\w*|savechanges|submitchanges|commit|executecommand|executenonquery|writetoserver|movefrom\w*)\s*\(").unwrap()
});
fn code(line: &str) -> bool {
    let s = line.trim();
    !s.is_empty() && !s.starts_with(['\'', '/', '*', '#'])
}
fn short(s: &str) -> String {
    s.trim().chars().take(300).collect()
}

pub(super) struct Observations<'a> {
    body: Vec<&'a str>,
    start: usize,
    rows: BTreeMap<(usize, String), u8>,
}
impl<'a> Observations<'a> {
    pub(super) fn new(body: &'a str, start: usize) -> Self {
        let mut rows = BTreeMap::new();
        // A database context type in the declaration establishes this boundary.
        static DB_PARAMETER: LazyLock<Regex> = LazyLock::new(|| Regex::new(
            r"(?ix)^(?:(?:optional|byval|byref|ref|in|out)\s+)*(?: (?P<vb>\w+)\s+as\s+(?:[\w.]*\.)?\w*(?:datacontext|dbcontext)\b | (?:[\w.]*\.)?\w*(?:datacontext|dbcontext)\b\??\s+(?P<cs>\w+)\b )"
        ).unwrap());
        let mut declaration = String::new();
        for line in body.lines().take(16) {
            declaration.push_str(line.trim().trim_end_matches(" _"));
            declaration.push(' ');
            if !declaration.contains('(') || declaration.matches('(').count() <= declaration.matches(')').count() {
                break;
            }
        }
        for parameter in arguments(&declaration) {
            if let Some(m) = DB_PARAMETER.captures(&parameter) {
                let name = m.name("vb").or_else(|| m.name("cs")).unwrap().as_str();
                let boundary = if parameter.contains('=') || parameter.to_ascii_lowercase().starts_with("optional ") {
                    "When supplied by the caller, preserve its commit boundary."
                } else {
                    "The caller supplies this parameter. Preserve its commit boundary."
                };
                rows.insert((start, format!("Database context parameter {name}: {boundary} Declaration: {}", short(&parameter))), 7);
            }
        }
        for (i, l) in body.lines().enumerate().skip(1).filter(|(_, l)| code(l)) {
            let lower = l.trim().to_lowercase();
            let priority = if WRITE.is_match(l) || ALLOC.is_match(l) || lower.contains(".dispose(")
            {
                3
            } else if lower.starts_with("if ")
                || lower.starts_with("if(")
                || lower.contains("rollback")
                || lower.contains("transaction")
            {
                1
            } else {
                continue;
            };
            rows.insert((start + i, short(l)), priority);
        }
        Self {
            body: body.lines().collect(),
            start,
            rows,
        }
    }
    /// A source-qualified spelling can resolve a direct call even when the
    /// bounded graph inventory omits it. Admit unique declarations only.
    pub(super) fn qualified_targets(
        &self,
        graph: &engram_graph::GraphStore,
        project: &str,
        known: &std::collections::HashSet<String>,
    ) -> Vec<engram_graph::Node> {
        static QUALIFIED: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"\b([A-Za-z_]\w*(?:\.[A-Za-z_]\w*)+)\s*\(").unwrap());
        let mut names = std::collections::BTreeSet::new();
        for line in self.body.iter().skip(1).filter(|l| code(l)) {
            for cap in QUALIFIED.captures_iter(line) {
                let name = cap[1].to_string();
                if known.contains(&name.to_ascii_lowercase()) {
                    continue;
                }
                names.insert(name);
            }
        }
        graph
            .unique_named_functions(project, names.iter().map(String::as_str))
            .unwrap_or_default()
            .into_values()
            .collect()
    }
    pub(super) fn callee(&mut self, name: &str, path: &str, start: usize, span: &[&str]) {
        let Some((decl, rest)) = span.split_first() else {
            return;
        };
        let leaf = name.rsplit(['.', ':']).next().unwrap_or(name);
        if leaf.len() < 2
            || !self
                .body
                .iter()
                .any(|l| l.to_ascii_lowercase().contains(&leaf.to_ascii_lowercase()))
        {
            return;
        }
        let pattern =
            Regex::new(&format!(r"(?i)\b{}\s*\(", regex::escape(leaf))).expect("escaped callee");
        let statements: Vec<_> = rest
            .iter()
            .enumerate()
            .filter(|(_, l)| code(l))
            .map(|(i, l)| (start + i + 1, l.trim()))
            .collect();
        let allocation = statements.iter().find(|(_, l)| ALLOC.is_match(l));
        let cleanup = statements.iter().find(|(_, l)| {
            let l = l.to_lowercase();
            l.contains(".dispose(")
                || (l.starts_with("using ") && (ALLOC.is_match(&l) || l.contains("reuseorcreate")))
        });
        let commit = statements.iter().find(|(_, l)| {
            let l = l.to_lowercase();
            [
                ".submitchanges(",
                ".savechanges(",
                ".commit(",
                ".writetoserver",
                ".executenonquery",
            ]
            .iter()
            .any(|term| l.contains(term))
        });
        let throw = statements
            .first()
            .filter(|(_, l)| l.to_lowercase().starts_with("throw "));
        if allocation.is_none() && cleanup.is_none() && commit.is_none() && throw.is_none() {
            return;
        }
        let later_dispose = self
            .body
            .iter()
            .enumerate()
            .find(|(_, s)| code(s) && s.to_lowercase().contains(".dispose("))
            .map(|(i, _)| self.start + i);
        let caller_allocates = self.body.iter().any(|s| code(s) && ALLOC.is_match(s));
        let parameter = arguments(decl).iter().enumerate().find_map(|(i, p)| {
            let lower = p.to_lowercase();
            let (left, ty) = lower.split_once(" as ")?;
            ty.split([' ', '='])
                .next()?
                .ends_with("context")
                .then(|| (i, left.split_whitespace().last().unwrap_or("").to_owned()))
        });
        for (offset, line) in self
            .body
            .iter()
            .enumerate()
            .skip(1)
            .filter(|(_, l)| code(l))
        {
            let Some(hit) = pattern.find(line) else {
                continue;
            };
            let prefix = &line[..hit.start()];
            // Distinct declarations may share a leaf (Create, Save, GetByID).
            // Never attach another receiver's effects to that call site.
            let qualifier = prefix
                .trim_end()
                .trim_end_matches('.')
                .rsplit(|c: char| !c.is_alphanumeric() && c != '_' && c != '.')
                .next()
                .unwrap_or("");
            if prefix.trim_end().ends_with('.') && !qualifier.is_empty() {
                let full = format!("{qualifier}.{leaf}").to_lowercase();
                let target = name.to_lowercase();
                if target != full && !target.ends_with(&format!(".{full}")) {
                    continue;
                }
            }
            let args = arguments(&line[hit.start()..]);
            let actual = parameter.as_ref().and_then(|(i, p)| {
                args.iter()
                    .find_map(|arg| {
                        let (k, v) = arg.split_once(":=")?;
                        k.trim()
                            .eq_ignore_ascii_case(p)
                            .then(|| v.trim().to_string())
                    })
                    .or_else(|| args.get(*i).filter(|v| !v.contains(":=")).cloned())
            });
            let n = self.start + offset;
            if let Some((at, statement)) = throw {
                let dispose=later_dispose.filter(|at|caller_allocates && *at>n)
                    .map_or(String::new(),|at|format!("; explicit caller Dispose at L{at} follows this call (check Using/Finally)"));
                self.rows.insert(
                    (
                        n,
                        format!(
                            "Conditional or direct exit via {name}: {path}:{at} {}{dispose}. Call: {}",
                            short(statement),
                            short(line)
                        ),
                    ),
                    6,
                );
            }
            if let Some((at, statement)) = allocation.or(cleanup) {
                let ownership = match (&parameter, &actual) {
                    (Some((_, p)), Some(v)) => format!("caller argument {v} for {p}"),
                    (Some((_, p)), None) => format!("no {p} argument shown at call site"),
                    _ => "callee resource".into(),
                };
                let close = cleanup.map_or_else(
                    || "no explicit Dispose or Using observed in this method".to_owned(),
                    |(i, s)| format!("cleanup evidence {path}:{i} {}", short(s)),
                );
                self.rows.insert(
                    (
                        n,
                        format!(
                            "{name} ({ownership}): {path}:{at} {}; {close}",
                            short(statement)
                        ),
                    ),
                    5,
                );
            }
            if let Some((at, statement)) = commit {
                let receiver = parameter
                    .as_ref()
                    .zip(actual.as_ref())
                    .filter(|((_, p), _)| statement.to_ascii_lowercase().contains(&format!("{p}.")))
                    .map_or(String::new(), |((_, p), v)| {
                        format!("; commit receiver {p} can be the caller-supplied {v}")
                    });
                self.rows.insert(
                    (
                        n,
                        format!(
                            "Callee {name} at {path}:{at}: {}{receiver}",
                            short(statement)
                        ),
                    ),
                    5,
                );
            }
        }
    }
    pub(super) fn finish(self) -> Vec<String> {
        let mut rows: Vec<_> = self.rows.into_iter().collect();
        rows.sort_by_key(|((line, _), weight)| (std::cmp::Reverse(*weight), *line));
        // Preserve the actual source statement even when its callees have a
        // commit, an allocation and an exit at that same line.
        let mut per_priority = [0usize; 8];
        rows.retain(|((_, _), weight)| {
            let count = &mut per_priority[*weight as usize];
            *count += 1;
            *count
                <= match weight {
                    1 => 6,
                    3 => 16,
                    5 => 18,
                    _ => 12,
                }
        });
        rows.truncate(48);
        rows.sort_by_key(|((line, _), _)| *line);
        rows.into_iter()
            .map(|((n, s), _)| format!("L{n} {s}"))
            .collect()
    }
}
/// Positional arguments through the end of this source line, with nested calls and strings.
fn arguments(s: &str) -> Vec<String> {
    let Some((_, tail)) = s.split_once('(') else {
        return vec![];
    };
    let (mut args, mut depth, mut quoted, mut from) = (Vec::new(), 0usize, false, 0usize);
    for (i, c) in tail.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '(' | '[' if !quoted => depth += 1,
            ')' | ']' if !quoted && depth > 0 => depth -= 1,
            ',' | ')' if !quoted && depth == 0 => {
                let part = tail[from..i].trim();
                if !part.is_empty() {
                    args.push(part.to_owned());
                }
                if c == ')' {
                    break;
                }
                from = i + 1;
            }
            _ => {}
        }
    }
    args
}
