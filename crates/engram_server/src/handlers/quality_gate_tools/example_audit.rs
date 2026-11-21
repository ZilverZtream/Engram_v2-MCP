//! Compile bounded, inspectable checks from the project's own policy examples.
mod catalog;
mod syntax;
pub(super) use catalog::audit;

use crate::services::{pre_commit_review_service::DiffFile, quality_source::SourceVerifier};
use engram_index::HybridSearchEngine;
use regex::Regex;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write,
    path::Path,
    sync::LazyLock,
};
use syntax::{Part, Shape, expressions, tokens};

#[derive(Default)]
pub(super) struct Report {
    pub summary: String,
    pub matches: String,
    pub inventory: String,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Check {
    shape: Shape,
    in_loop: bool,
    companion: Option<Shape>,
}
impl Check {
    fn new(shape: Shape) -> Self {
        Self {
            shape,
            in_loop: false,
            companion: None,
        }
    }
}
static SPAN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`([^`\n]+)`").unwrap());
static BAD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:bad|incorrect|wrong)(?: example)?:\s*").unwrap());
static GOOD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:good|correct|right)(?: example)?:\s*").unwrap());
static WITH_EXAMPLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(new\s+[A-Za-z_$][\w$]*|(?:[A-Za-z_$][\w$]*\.)+[A-Za-z_$][\w$]*)\(\.\.\.\)")
        .unwrap()
});
static TAGS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([A-Za-z#/. ,_-]+)\]\s*$").unwrap());

fn paired(policy: &str, bad: &str, good: &str, checks: &mut BTreeSet<Check>) {
    let lower = policy.to_lowercase();
    let vb = lower.contains("vb.net")
        || good.contains(" As ")
        || bad.contains("Dim ")
        || bad.contains("For Each");
    let good_tokens: Vec<_> = good.lines().flat_map(|s| tokens(s, vb)).collect();
    for line in bad.lines() {
        let line = line.split("//").next().unwrap_or(line);
        let t = tokens(line, vb);
        for shape in expressions(&t) {
            if shape.matches(&good_tokens, vb) {
                continue;
            }
            // If the rule describes an omission, an isolated call is insufficient.
            if [
                "without ",
                "before ",
                "outside ",
                "unless ",
                "only when ",
                "unhandled ",
                "no error",
                "never leave",
                "try/catch",
                "must supply",
                "missing ",
                "on every",
                "if it ",
            ]
            .iter()
            .any(|w| lower.contains(w))
            {
                continue;
            }
            checks.insert(Check::new(shape));
        }
        // A demonstrated property assignment to a different member than the good example.
        for (i, tok) in t.iter().enumerate() {
            if tok != "=" || i < 2 || t[i - 2] != "." {
                continue;
            }
            let rhs = &t[i + 1..];
            let last = rhs.windows(2).filter(|w| w[0] == ".").last();
            if let Some(pair) = last {
                let field = &t[i - 1];
                let member = &pair[1];
                if !good_tokens.iter().any(|v| v.eq_ignore_ascii_case(field))
                    || good_tokens.iter().any(|v| v.eq_ignore_ascii_case(member))
                {
                    continue;
                }
                // Anchor both sides separately; the receiver can be qualified.
                let key = format!("@assignment:{field}:{member}");
                checks.insert(Check {
                    shape: Shape(vec![Part::Exact(key)]),
                    in_loop: bad.to_ascii_lowercase().contains("for each"),
                    companion: None,
                });
            }
        }
    }
    // Positive and negative examples of user-visible string properties also apply
    // to object initializers assigning that same property. Keep the property name.
    if (lower.contains("localiz") || lower.contains("resource"))
        && (lower.contains("literal") || lower.contains("hardcod"))
    {
        static PROPERTY: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r#"(?i)([a-z_$][\w$]*)\s*[:=]\s*["'][^"'<>]*[a-z][^"'<>]*["']"#).unwrap()
        });
        for p in PROPERTY.captures_iter(bad) {
            for sep in [":", "="] {
                checks.insert(Check::new(Shape(vec![
                    Part::Exact(p[1].to_owned()),
                    Part::Exact(sep.into()),
                    Part::Text,
                ])));
            }
        }
    }
    // Parenthesized calls followed by a member observed only in the bad example.
    let bad_tokens = tokens(bad, vb);
    for w in bad_tokens.windows(3) {
        if w[0] == ")"
            && w[1] == "."
            && policy
                .to_lowercase()
                .contains(&format!(".{}", w[2].to_lowercase()))
        {
            let pat = Shape::literal(w);
            if !pat.matches(&good_tokens, vb) {
                checks.insert(Check::new(pat));
            }
        }
    }
}

fn directives(text: &str, checks: &mut BTreeSet<Check>) {
    let lower = text.to_lowercase();
    // Use explicit source alternatives. Suppress conditional directives unless
    // the lexical context can actually be checked.
    // VB intrinsics expose different parameter roles: the length in Left/Right
    // is not a start position. Bind an actual zero start only for a cited intrinsic.
    if lower.contains("1-based") || lower.contains("one-based") {
        for span in SPAN.captures_iter(text) {
            let name = span[1].trim_end_matches("()").to_lowercase();
            if matches!(name.as_str(), "mid" | "mid$" | "instr" | "instrrev") {
                checks.insert(Check::new(Shape(vec![Part::Exact(format!(
                    "@vb-start:{name}"
                ))])));
            }
        }
    }
    let table = text.trim_start().starts_with('|');
    let subject = if table {
        text.trim()
            .trim_matches('|')
            .split('|')
            .next()
            .unwrap_or("")
    } else {
        text
    };
    let sl = subject.to_lowercase();
    let in_loop = sl.contains("loop")
        && (sl.contains("inside") || sl.contains("within") || sl.contains("in a "));
    let simple = table
        && ["read ", "use ", "call "]
            .iter()
            .any(|a| sl.trim().starts_with(a))
        && (!sl.contains(" in ") || in_loop)
        && ![
            "without", "before", "after", "when", " if ", " on ", " to ", "outside",
        ]
        .iter()
        .any(|v| sl.contains(v));
    static DENY: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)(?:do(?:es)? not|never|don't|must not|avoid|forbid(?:den)?|disallow(?:ed)?)\s+(?:(?:add|use|call|read|introduce|invoke|allow|write|new|any|direct|an?|the|es\d+|module)\s+)*$").unwrap()
    });
    for span in SPAN.captures_iter(subject) {
        let m = span.get(0).unwrap();
        let prefix = subject[..m.start()].trim_end();
        if !simple && !DENY.is_match(prefix) {
            continue;
        }
        let label = &span[1];
        let tok = tokens(label, false);
        if tok.iter().any(|v| v == "/" || v == "\\") || tok.is_empty() {
            continue;
        }
        if !syntax::has_code_fragment(label)
            && !matches!(label, "import" | "export" | "require" | "await")
        {
            continue;
        }
        if matches!(label, "import" | "export") {
            checks.insert(Check::new(Shape(vec![Part::Exact(format!(
                "@keyword:{label}"
            ))])));
            continue;
        }
        let mut pat = tok;
        if pat.ends_with(&["(".into(), ")".into()]) {
            pat.pop();
        }
        checks.insert(Check {
            shape: Shape::literal(&pat),
            in_loop,
            companion: None,
        });
    }
    if lower.contains("non-")
        && (lower.contains("incompatib")
            || lower.contains("runtime api")
            || lower.contains("unsupported"))
    {
        for m in WITH_EXAMPLE.captures_iter(text) {
            let name = &m[1];
            let anchor = if let Some((_, member)) = name.rsplit_once('.') {
                format!(".{member}(")
            } else {
                format!("{name}(")
            };
            checks.insert(Check::new(Shape::literal(&tokens(&anchor, false))));
        }
    }
}

fn compile(text: &str) -> BTreeSet<Check> {
    let mut checks = BTreeSet::new();
    if let Some(b) = BAD.find(text)
        && let Some(g) = GOOD.find_at(text, b.end())
    {
        paired(
            &text[..b.start()],
            text[b.end()..g.start()].trim_end_matches([' ', '—']),
            &text[g.end()..],
            &mut checks,
        );
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(text)
        && let (Some(b), Some(g)) = (v["bad_example"].as_str(), v["good_example"].as_str())
    {
        paired(v["rule"].as_str().unwrap_or(""), b, g, &mut checks);
    }
    // Interpret only a table explicitly headed as prohibitions/alternatives.
    let mut bad_table = false;
    for line in text.lines() {
        let lower = line.trim().to_lowercase();
        if lower.starts_with('|') {
            if lower.contains("never do")
                || lower.starts_with("| bad ")
                || lower.starts_with("| avoid ")
            {
                bad_table = true;
                continue;
            }
            if !bad_table {
                continue;
            }
        } else {
            bad_table = false
        }
        directives(line, &mut checks);
    }
    checks
}

struct Input<'a> {
    file: &'a DiffFile,
    vb: bool,
    lines: Vec<(usize, Vec<String>, usize, usize)>,
}
impl<'a> Input<'a> {
    fn new(file: &'a DiffFile) -> Self {
        let vb = file.path.to_lowercase().ends_with(".vb");
        let mut lines = Vec::new();
        let mut depth = 0usize;
        let mut group = 0usize;
        let mut previous = 0;
        let mut block_comment = false;
        for (no, source) in &file.added_lines {
            if *no != previous + 1 {
                depth = 0;
                group += 1;
                block_comment = false
            }
            previous = *no;
            let mut src = source.as_str();
            if block_comment {
                if let Some((_, rest)) = src.split_once("*/") {
                    src = rest;
                    block_comment = false
                } else {
                    continue;
                }
            }
            let t = tokens(src, vb);
            if !vb && src.trim_start().starts_with("/*") && !src.contains("*/") {
                block_comment = true
            }
            let first = t.first().map(String::as_str).unwrap_or("");
            if matches!(first, "next" | "loop" | "wend")
                || (first == "end" && t.get(1).is_some_and(|s| s == "while"))
            {
                depth = depth.saturating_sub(1)
            }
            if matches!(first, "for" | "foreach" | "while" | "do") {
                depth += 1
            }
            if first == "end" && t.get(1).is_some_and(|s| s == "sub" || s == "function")
                || first == "}" && t.len() == 1
            {
                group += 1;
                if !vb {
                    depth = 0
                }
            }
            lines.push((*no, t, depth, group));
        }
        Self { file, vb, lines }
    }
    fn hits(&self, c: &Check) -> Vec<usize> {
        self.lines
            .iter()
            .filter_map(|(line, t, depth, group)| {
                if c.in_loop && *depth == 0 {
                    return None;
                }
                let special = c.shape.0.first().and_then(|p| match p {
                    Part::Exact(s) => Some(s.as_str()),
                    _ => None,
                });
                let hit = if let Some(key) = special.and_then(|s| s.strip_prefix("@keyword:")) {
                    t.first().is_some_and(|s| s == key)
                        && t.get(1).is_some_and(|s| s != "(" && s != ".")
                } else if let Some(name) = special.and_then(|s| s.strip_prefix("@vb-start:")) {
                    self.vb
                        && t.windows(2).enumerate().any(|(i, w)| {
                            if !w[0].eq_ignore_ascii_case(name) || w[1] != "(" {
                                return false;
                            }
                            let mut args = vec![Vec::new()];
                            let mut depth = 0;
                            for token in &t[i + 2..] {
                                if token == ")" && depth == 0 {
                                    break;
                                }
                                if token == "," && depth == 0 {
                                    args.push(Vec::new());
                                    continue;
                                }
                                if token == "(" {
                                    depth += 1
                                } else if token == ")" {
                                    depth -= 1
                                }
                                args.last_mut().unwrap().push(token.as_str());
                            }
                            let pos = match name {
                                "mid" | "mid$" => 1,
                                "instr" if args.len() >= 3 => 0,
                                "instrrev" => 2,
                                _ => return false,
                            };
                            args.get(pos).is_some_and(|arg| arg == &["0"])
                        })
                } else if let Some(pair) = special.and_then(|s| s.strip_prefix("@assignment:")) {
                    let (field, member) = pair.split_once(':')?;
                    t.windows(3).enumerate().any(|(i, w)| {
                        w[0] == "."
                            && w[1].eq_ignore_ascii_case(field)
                            && w[2] == "="
                            && t[i + 3..]
                                .windows(2)
                                .take_while(|v| v[0] != "," && v[0] != ";")
                                .any(|w| w[0] == "." && w[1].eq_ignore_ascii_case(member))
                    })
                } else {
                    c.shape.matches(t, self.vb)
                };
                if !hit {
                    return None;
                }
                if let Some(other) = &c.companion
                    && self
                        .lines
                        .iter()
                        .any(|(_, l, _, g)| g == group && other.matches(l, self.vb))
                {
                    return None;
                }
                Some(*line)
            })
            .collect()
    }
}
