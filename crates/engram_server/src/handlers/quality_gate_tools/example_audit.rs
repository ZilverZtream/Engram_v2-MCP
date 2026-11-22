//! Compile bounded, inspectable checks from the project's own policy examples.
mod catalog;
mod structural;
mod syntax;
pub(super) use catalog::audit;
pub(super) use catalog::document_applies;

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
    fn describe(&self) -> String {
        let display = |shape: &Shape| {
            shape
                .0
                .iter()
                .map(|v| match v {
                    Part::Exact(t) => t.as_str(),
                    Part::Name => "<name>",
                    Part::Argument => "<argument>",
                    Part::Text => "<literal>",
                    Part::Assignment => "<assignment>",
                    Part::Local(_) => "<local>",
                })
                .collect::<Vec<_>>()
                .join(" ")
        };
        let mut text = display(&self.shape);
        if let Some(pair) = text.strip_prefix("@assignment:") {
            text = format!("assignment {} ", pair.replace(":", " from member "));
        }
        if let Some(key) = text.strip_prefix("@keyword:") {
            text = format!("module keyword {key}");
        }
        if let Some(name) = text.strip_prefix("@vb-start:") {
            text = format!("{name} with a zero start position");
        }
        if self.in_loop {
            text.push_str(" inside the added loop");
        }
        if let Some(companion) = &self.companion {
            text.push_str(&format!(
                " without visible companion {}",
                display(companion)
            ));
        }
        text.chars().take(180).collect()
    }
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
    let meaningful: Vec<_> = bad
        .lines()
        .map(|line| tokens(line, vb))
        .filter(|t| !t.is_empty())
        .collect();
    let whole: Vec<_> = meaningful.iter().flatten().cloned().collect();
    // Multi-statement contracts and non-call examples retain their surrounding
    // syntax. Only compile a contrasting example with a concrete code anchor.
    let complex = meaningful.len() > 1 || whole.iter().filter(|v| v.as_str() == ";").count() > 1;
    if (6..=100).contains(&whole.len())
        && whole
            .iter()
            .any(|s| matches!(s.as_str(), "." | "::" | "(" | "=>"))
        && !whole.windows(3).any(|w| w == [".", ".", "."])
        && !whole.iter().any(|s| s == "?")
    {
        let mut shape = Shape::literal(&whole);
        shape.bind_locals(&whole);
        if !shape.matches(&good_tokens, vb) {
            checks.insert(Check::new(shape));
        }
    }
    for line in bad.lines() {
        let line = line.split("//").next().unwrap_or(line);
        let t = tokens(line, vb);
        for shape in expressions(&t) {
            // A negative example can use an otherwise harmless operation as
            // part of an assignment, condition or argument. Keep that context
            // unless both examples call the same operation with different
            // arguments, or the example is the call itself.
            let statement = t.strip_suffix(&[";".to_owned()]).unwrap_or(&t);
            let standalone = statement.len() == shape.0.len() && shape.matches(statement, vb);
            let mut operation = Vec::new();
            for part in &shape.0 {
                operation.push(part.clone());
                if matches!(part,Part::Exact(s) if s=="(") {
                    break;
                }
            }
            // If the positive example changes only the call, its unchanged
            // surroundings are evidence that the operation itself is the contrast.
            let replaced = statement
                .windows(shape.0.len())
                .position(|w| shape.matches(w, vb))
                .is_some_and(|at| {
                    let prefix = &statement[..at];
                    let suffix = &statement[at + shape.0.len()..];
                    good.lines().flat_map(|line| line.split(';')).any(|line| {
                        let g = tokens(TAGS.replace(line.trim(), "").trim(), vb);
                        let Some(inner) = g
                            .strip_prefix(prefix)
                            .and_then(|rest| rest.strip_suffix(suffix))
                        else {
                            return false;
                        };
                        expressions(inner).iter().any(|candidate| {
                            candidate.0.len() == inner.len() && candidate.matches(inner, vb)
                        })
                    })
                });
            if !standalone && !replaced && !Shape(operation).matches(&good_tokens, vb) {
                continue;
            }

            if complex || shape.matches(&good_tokens, vb) {
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
            checks.insert(Check::new(Shape(vec![
                Part::Exact(p[1].to_owned()),
                Part::Assignment,
                Part::Text,
            ])));
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
        let prefix = &subject[..m.start()];
        if !simple && !DENY.is_match(prefix) {
            continue;
        }
        let suffix = subject[m.end()..].trim_start().to_ascii_lowercase();
        if !simple
            && [
                "on ", "from ", "before ", "after ", "if ", "when ", "unless ", "outside ",
                "inside ", "in ", "without ", "only ", "for ", "to ",
            ]
            .iter()
            .any(|condition| suffix.starts_with(condition))
        {
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
    if let Some(b) = BAD.find(text)
        && let Some(g) = GOOD.find_at(text, b.end())
    {
        let bad = text[b.end()..g.start()]
            .trim()
            .trim_end_matches('\u{2014}')
            .trim_end();
        let good = &text[g.end()..];
        static ALTERNATE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"(?i);\s+or\s+(?:in [a-z0-9 ]+:\s*)?").unwrap());
        let bad = ALTERNATE.replace_all(bad, "\n");
        let good = ALTERNATE.replace_all(good, "\n");
        let rows = |s: &str| {
            s.lines()
                .enumerate()
                .map(|(n, l)| (n + 1, l.to_owned()))
                .collect::<Vec<_>>()
        };
        let bad_rows = rows(&bad);
        let good_rows = rows(&good);
        let lower = text.to_lowercase();
        let vb = lower.contains("vbnet") || bad.contains("For Each") || bad.contains("Dim ");
        let js = lower.contains("javascript") || lower.contains("typescript") || !vb;
        let grammars = [("example.vb", vb), ("example.ts", js)]
            .into_iter()
            .filter(|(_, enabled)| *enabled)
            .map(|(path, _)| {
                (
                    Input::from_lines(path, &bad_rows),
                    Input::from_lines(path, &good_rows),
                )
            })
            .collect::<Vec<_>>();
        checks.retain(|c| {
            grammars.iter().any(|(bad, _)| !bad.hits(c).is_empty())
                && grammars.iter().all(|(_, good)| good.hits(c).is_empty())
        });
    }
    checks
}

struct Input<'a> {
    path: &'a str,
    vb: bool,
    lines: Vec<(usize, Vec<String>, usize, usize)>,
}
impl<'a> Input<'a> {
    fn new(file: &'a DiffFile) -> Self {
        Self::from_lines(&file.path, &file.added_lines)
    }
    fn from_lines(path: &'a str, rows: &[(usize, String)]) -> Self {
        let vb = path.to_lowercase().ends_with(".vb");
        let mut lines = Vec::new();
        let mut depth = 0usize;
        let mut group = 0usize;
        let mut previous = 0;
        let mut block_comment = false;
        for (no, source) in rows {
            if *no != previous + 1 {
                depth = 0;
                group += 1;
                block_comment = false
            }
            previous = *no;
            let t = syntax::source_tokens(
                source,
                vb,
                path.rsplit('.').next().unwrap_or(""),
                &mut block_comment,
            );
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
        Self { path, vb, lines }
    }
    fn hits(&self, c: &Check) -> Vec<usize> {
        self.lines
            .iter()
            .enumerate()
            .filter_map(|(index, (line, t, depth, group))| {
                if c.in_loop && *depth == 0 {
                    return None;
                }
                let special = c.shape.0.first().and_then(|p| match p {
                    Part::Exact(s) => Some(s.as_str()),
                    _ => None,
                });
                let hit = if let Some(key) = special.and_then(|s| s.strip_prefix("@keyword:")) {
                    matches!(
                        self.path.rsplit('.').next(),
                        Some("js" | "ts" | "tsx" | "jsx" | "mjs" | "mts")
                    ) && t.first().is_some_and(|s| s == key)
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
                    // For a longer statement or expression continued on the next line, match
                    // from this line into adjacent submitted lines only.
                    if c.shape.matches(t, self.vb) {
                        true
                    } else if c.shape.0.len() > 1 && !t.is_empty() {
                        let mut span = t.clone();
                        let mut previous = *line;
                        for (next, row, _, _) in self.lines[index + 1..].iter() {
                            if *next != previous + 1 || span.len() >= t.len() + c.shape.0.len() - 1
                            {
                                break;
                            }
                            span.extend(
                                row.iter()
                                    .take(t.len() + c.shape.0.len() - 1 - span.len())
                                    .cloned(),
                            );
                            previous = *next;
                        }
                        c.shape.matches_from(&span, t.len(), self.vb)
                    } else {
                        false
                    }
                };
                if !hit {
                    return None;
                }
                // A visible body scroll-lock companion applies to a dialog
                // receiver. The same visibility operation on a row, menu
                // item or validation message is ordinary UI state.
                if c.companion.as_ref().is_some_and(|other| other.0.iter().any(|p|
                    matches!(p, Part::Exact(s) if s.trim_matches(['\'', '"']) == "modal-open")))
                    && !t.windows(3).any(|w| w[1] == "." && w[2] == "classList"
                        && super::diff_checks::dialog_receiver(&w[0])) {
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn policy_changes_which_call_is_detected() {
        let before = "Use the supported client. Bad: Clock.discard(job); Good: Clock.archive(job); [typescript]";
        let after = "Use the supported client. Bad: Clock.archive(job); Good: Clock.discard(job); [typescript]";
        for (policy, found, clean) in [
            (before, "discard", "archive"),
            (after, "archive", "discard"),
        ] {
            let patterns = compile(policy);
            let matches = |source: &str| {
                patterns
                    .iter()
                    .any(|c| c.shape.matches(&tokens(source, false), false))
            };
            assert!(matches(&format!("const value = Clock.{found}(payload);")));
            assert!(!matches(&format!(
                "Clock.{clean}(payload); // Clock.{found}(payload)"
            )));
            assert!(!matches(&format!("note = \"Clock.{found}(payload)\";")));
        }
    }
    #[test]
    fn an_embedded_call_retains_its_assignment_context() {
        let policy = "Convert typed values explicitly. Bad: record.Payload = src.Format() Good: record.Payload = src.Data [typescript]";
        let rules = compile(policy);
        let matches = |source: &str| {
            rules
                .iter()
                .any(|c| c.shape.matches(&tokens(source, false), false))
        };
        assert!(matches("record.Payload = src.Format()"));
        assert!(!matches("labels.push(id.Format())"));
    }
    #[test]
    fn a_replacement_call_preserves_nested_argument_support() {
        let rules = compile(
            "Use the supported formatter. Bad: SELECT LegacyForm(item) FROM staging; Good: SELECT LocalForm(item) FROM staging; [sql]",
        );
        let matches = |src: &str| {
            rules
                .iter()
                .any(|c| c.shape.matches(&tokens(src, false), false))
        };
        assert!(matches(
            "SELECT LegacyForm(Trim(display_name)) FROM ledger;"
        ));
        assert!(!matches(
            "SELECT LocalForm(Trim(display_name)) FROM ledger;"
        ));
    }
    #[test]
    fn declared_locals_can_be_renamed_but_must_keep_their_relationship() {
        let rules = compile(
            "Keep the results distinct. Bad: var buffer = read(); var buffer = flush(); Good: var first = read(); var last = flush(); [javascript]",
        );
        let matches = |src: &str| {
            rules
                .iter()
                .any(|c| c.shape.matches(&tokens(src, false), false))
        };
        assert!(matches("var response = read(); var response = flush();"));
        assert!(!matches("var response = read(); var receipt = flush();"));
    }
    #[test]
    fn conditional_prose_does_not_ban_a_method_globally() {
        let text = "Never call `Persist()` on a borrowed session.";
        assert!(compile(text).is_empty());
    }
}
