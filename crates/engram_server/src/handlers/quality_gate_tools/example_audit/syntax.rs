//! Reusable lexical templates. Rule text is data and is never executed.
use regex::Regex;
use std::sync::LazyLock;

static LEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)/\*.*?(?:\*/|\z)|//[^\n]*|"(?:\\.|""|[^"\\\n])*"|'(?:\\.|[^'\\\n])*'|`[^`\n]*`|[[:alpha:]_$][\w$]*|\d+(?:\.\d+)?|=>|::|[^\s]"#).unwrap()
});

pub(super) fn tokens(s: &str, vb: bool) -> Vec<String> {
    source_tokens(s, vb, "", &mut false)
}
pub(super) fn source_tokens<'a>(
    mut s: &'a str,
    vb: bool,
    extension: &str,
    block: &mut bool,
) -> Vec<String> {
    if *block {
        let Some((_, tail)) = s.split_once("*/") else {
            return Vec::new();
        };
        s = tail;
        *block = false;
    }
    let mut out = Vec::new();
    for m in LEX.find_iter(s) {
        let t = m.as_str();
        if vb && t.starts_with('\'') {
            break;
        }
        if t == "#" && matches!(extension, "py" | "sh" | "rb" | "yaml" | "yml")
            || matches!(extension, "sql" | "lua") && s[m.start()..].starts_with("--")
        {
            break;
        }
        if t.starts_with("/*") && !t.ends_with("*/") {
            *block = true
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
    Argument,
    Text,
    Assignment,
}
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(super) struct Shape(pub Vec<Part>);
impl Shape {
    pub(super) fn literal(s: &[String]) -> Self {
        Self(s.iter().cloned().map(Part::Exact).collect())
    }
    pub(super) fn matches(&self, src: &[String], vb: bool) -> bool {
        self.matches_from(src, src.len(), vb)
    }
    pub(super) fn matches_from(&self, src: &[String], starts: usize, vb: bool) -> bool {
        if self.0.is_empty() {
            return false;
        }
        (0..starts.min(src.len())).any(|start| {
            let mut i = start;
            for part in &self.0 {
                let Some(t) = src.get(i) else { return false };
                if *part == Part::Argument {
                    let first = i;
                    let mut depth = 0;
                    while let Some(v) = src.get(i) {
                        if (depth == 0 && matches!(v.as_str(), "," | ")" | ";")) || i - first >= 100
                        {
                            break;
                        }
                        if matches!(v.as_str(), "(" | "[" | "{") {
                            depth += 1
                        } else if matches!(v.as_str(), ")" | "]" | "}") {
                            if depth == 0 {
                                break;
                            }
                            depth -= 1
                        }
                        i += 1;
                    }
                    if first == i || depth != 0 {
                        return false;
                    }
                    continue;
                }
                if !match part {
                    Part::Exact(p) if vb && !quoted(p) => p.eq_ignore_ascii_case(t),
                    Part::Exact(p) if quoted(p) && quoted(t) => {
                        p.get(1..p.len().saturating_sub(1)) == t.get(1..t.len().saturating_sub(1))
                    }
                    Part::Exact(p) => p == t,
                    Part::Name => word(t),
                    Part::Text => {
                        quoted(t) && t.chars().any(|c| c.is_alphabetic()) && !t.contains("<%")
                    }
                    Part::Assignment => matches!(t.as_str(), ":" | "="),
                    Part::Argument => unreachable!(),
                } {
                    return false;
                }
                i += 1;
            }
            true
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
                p.0[k - start] = Part::Argument
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
