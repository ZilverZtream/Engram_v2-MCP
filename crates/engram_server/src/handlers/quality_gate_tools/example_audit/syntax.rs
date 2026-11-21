//! Reusable lexical templates. Rule text is data and is never executed.
use regex::Regex;
use std::sync::LazyLock;

static LEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)/\*.*?(?:\*/|\z)|//[^\n]*|"(?:\\.|""|[^"\\\n])*"|'(?:\\.|[^'\\\n])*'|`[^`\n]*`|[[:alpha:]_$][\w$]*|\d+(?:\.\d+)?|=>|::|[^\s]"#).unwrap()
});

pub(super) fn tokens(s: &str, vb: bool) -> Vec<String> {
    let mut out = Vec::new();
    for m in LEX.find_iter(s) {
        let t = m.as_str();
        if vb && t.starts_with('\'') {
            break;
        }
        if t.starts_with("//") || t.starts_with("/*") {
            continue;
        }
        out.push(if vb && !quoted(t) {
            t.to_ascii_lowercase()
        } else {
            t.to_owned()
        });
    }
    out
}
fn word(s: &str) -> bool {
    s.starts_with(|c: char| c.is_alphabetic() || c == '_' || c == '$')
}
fn quoted(s: &str) -> bool {
    s.starts_with(['"', '\'', '`'])
}
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(super) enum Part {
    Exact(String),
    Name,
    Text,
}
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(super) struct Shape(pub Vec<Part>);
impl Shape {
    pub(super) fn literal(s: &[String]) -> Self {
        Self(s.iter().cloned().map(Part::Exact).collect())
    }
    pub(super) fn matches(&self, src: &[String], vb: bool) -> bool {
        !self.0.is_empty()
            && src.windows(self.0.len()).any(|w| {
                w.iter().zip(&self.0).all(|(actual, p)| match p {
                    Part::Exact(t) if vb && !quoted(t) => actual.eq_ignore_ascii_case(t),
                    Part::Exact(t) if quoted(t) && quoted(actual) => {
                        t.get(1..t.len().saturating_sub(1))
                            == actual.get(1..actual.len().saturating_sub(1))
                    }
                    Part::Exact(t) => actual == t,
                    Part::Name => word(actual),
                    Part::Text => {
                        quoted(actual)
                            && actual.chars().any(|c| c.is_alphabetic())
                            && !actual.contains("<%")
                    }
                })
            })
    }
}

/// Preserve the operation, members, constants and arity. Only standalone
/// argument names vary; an example cannot turn a whole expression into a wildcard.
pub(super) fn expressions(s: &[String]) -> Vec<Shape> {
    let mut out = Vec::new();
    for i in 0..s.len() {
        if !word(&s[i]) {
            continue;
        }
        if s.get(i + 1).map(String::as_str) != Some("(") {
            continue;
        }
        let mut start = i;
        while start >= 2 && s[start - 1] == "." && word(&s[start - 2]) {
            start -= 2
        }
        if start > 0 && s[start - 1].eq_ignore_ascii_case("new") {
            start -= 1
        }
        // Exclude control constructs and definitions.
        if [
            "if", "while", "for", "function", "sub", "catch", "using", "switch",
        ]
        .iter()
        .any(|k| s[i].eq_ignore_ascii_case(k))
            || start.checked_sub(1).is_some_and(|p| {
                ["sub", "function", "class", "fn", "def"]
                    .iter()
                    .any(|k| s[p].eq_ignore_ascii_case(k))
            })
        {
            continue;
        }
        let mut depth = 0;
        let mut end = None;
        for (j, t) in s.iter().enumerate().skip(i + 1) {
            if t == "(" {
                depth += 1
            } else if t == ")" {
                depth -= 1;
                if depth == 0 {
                    end = Some(j);
                    break;
                }
            }
        }
        let Some(end) = end else { continue };
        if end - start > 55 {
            continue;
        }
        let mut p = Shape::literal(&s[start..=end]);
        if start < i
            && s[start + 1] == "."
            && s[start].starts_with(|c: char| c.is_lowercase())
            && s[start] != "this"
        {
            p.0[0] = Part::Name;
        }
        for k in i + 2..end {
            if word(&s[k])
                && matches!(s[k - 1].as_str(), "(" | ",")
                && matches!(s[k + 1].as_str(), ")" | ",")
                && !["null", "true", "false", "nothing"]
                    .iter()
                    .any(|v| s[k].eq_ignore_ascii_case(v))
            {
                p.0[k - start] = Part::Name
            }
        }
        out.push(p)
    }
    out
}

pub(super) fn has_code_fragment(s: &str) -> bool {
    let t = tokens(s, false);
    !t.is_empty()
        && t.len() <= 24
        && t.iter()
            .any(|v| matches!(v.as_str(), "." | "(" | ":" | "=" | "::" | "{") || v == "new")
}
