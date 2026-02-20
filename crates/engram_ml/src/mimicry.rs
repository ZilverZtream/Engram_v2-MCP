use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, Query, QueryCursor};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StyleGuide {
    pub bullets: Vec<String>,
    pub confidence: f32,
    pub evidence_snippets: Vec<String>,
}

impl StyleGuide {
    pub fn as_text(&self) -> String {
        if self.bullets.is_empty() {
            return "INSUFFICIENT_DATA".into();
        }
        let mut out = format!("Confidence: {:.2}\n\n", self.confidence);
        for b in &self.bullets {
            out.push_str(&format!("- {b}\n"));
        }
        if !self.evidence_snippets.is_empty() {
            out.push_str("\nEvidence Snippets:\n");
            for s in &self.evidence_snippets {
                out.push_str("---\n");
                out.push_str(s);
                out.push('\n');
            }
        }
        out
    }
}

#[derive(Clone, Default)]
pub struct StyleMimicryEngine;

impl StyleMimicryEngine {
    pub fn new() -> Self {
        Self
    }

    /// Extract a compact, actionable style guide from recent diffs and optionally the current file text.
    pub fn analyze(&self, diffs: &[String], current_file: Option<&str>) -> StyleGuide {
        let text = {
            let mut s = String::new();
            for d in diffs {
                s.push_str(d);
                s.push('\n');
            }
            if let Some(cur) = current_file {
                s.push_str(cur);
            }
            s
        };

        if text.trim().len() < 200 {
            return StyleGuide {
                bullets: vec![],
                confidence: 0.0,
                evidence_snippets: vec![],
            };
        }

        let mut bullets = Vec::new();
        let mut detections = 0;

        // 1. AST-based semantic analysis
        let (ast_bullets, ast_detections) = self.semantic_analyze(&text);
        bullets.extend(ast_bullets);
        detections += ast_detections;

        // 2. Lexical/Text-based fallback/additions
        if let Some(i) = detect_indent(&text) {
            bullets.push(i);
            detections += 1;
        }

        if let Some(e) = detect_error_handling(&text) {
            bullets.push(e);
            detections += 1;
        }

        if let Some(i) = detect_import_style(&text) {
            bullets.push(i);
            detections += 1;
        }

        if let Some(d) = detect_docs(&text) {
            bullets.push(d);
            detections += 1;
        }

        if let Some(l) = detect_logging(&text) {
            bullets.push(l);
            detections += 1;
        }

        if let Some(t) = detect_testing(&text) {
            bullets.push(t);
            detections += 1;
        }

        if let Some(n) = detect_naming(&text) {
            bullets.push(n);
            detections += 1;
        }

        let confidence = (detections as f32 / 5.0).min(1.0);

        // Evidence window: pick a few interesting lines
        let mut evidence_snippets = Vec::new();
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        if lines.len() >= 3 {
            // Take 3 snippets of 3 lines each
            for i in (0..lines.len().saturating_sub(3))
                .step_by(lines.len() / 3)
                .take(3)
            {
                evidence_snippets.push(lines[i..i + 3].join("\n"));
            }
        }

        bullets.truncate(5);
        StyleGuide {
            bullets,
            confidence,
            evidence_snippets,
        }
    }

    fn semantic_analyze(&self, text: &str) -> (Vec<String>, usize) {
        let mut bullets = Vec::new();
        let mut count = 0;

        // Try Rust
        if let Some((b, c)) = self.analyze_rust(text) {
            bullets.extend(b);
            count += c;
        }
        // Try Python
        if let Some((b, c)) = self.analyze_python(text) {
            bullets.extend(b);
            count += c;
        }

        (bullets, count)
    }

    fn analyze_rust(&self, text: &str) -> Option<(Vec<String>, usize)> {
        if !text.contains("fn ") && !text.contains("struct ") {
            return None;
        }

        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .ok()?;
        let tree = parser.parse(text, None)?;

        let query = Query::new(
            &tree_sitter_rust::LANGUAGE.into(),
            r#"
            (function_item (visibility_modifier)? @vis)
            (struct_item (visibility_modifier)? @vis)
            (call_expression function: (identifier) @call_name)
            (match_expression) @match
            (if_let_expression) @if_let
            "#,
        )
        .ok()?;

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, tree.root_node(), text.as_bytes());

        let mut pub_count = 0;
        let mut match_count = 0;
        let mut if_let_count = 0;

        while let Some(m) = matches.next() {
            for cap in m.captures {
                let name = query.capture_names()[cap.index as usize];
                match name {
                    "vis" => pub_count += 1,
                    "match" => match_count += 1,
                    "if_let" => if_let_count += 1,
                    _ => {}
                }
            }
        }

        let mut out = Vec::new();
        if match_count > 2 {
            out.push(
                "Prefer exhaustive pattern matching (`match`) over multiple `if` statements."
                    .into(),
            );
        }
        if if_let_count > 1 {
            out.push("Use `if let` for concise optional/result handling.".into());
        }
        if pub_count == 0 && (text.contains("fn ") || text.contains("struct ")) {
            out.push("Prefer private-by-default visibility for internal logic.".into());
        }

        if out.is_empty() {
            None
        } else {
            Some((out, 1))
        }
    }

    fn analyze_python(&self, text: &str) -> Option<(Vec<String>, usize)> {
        if !text.contains("def ") && !text.contains("class ") {
            return None;
        }

        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .ok()?;
        let tree = parser.parse(text, None)?;

        let query = Query::new(
            &tree_sitter_python::LANGUAGE.into(),
            r#"
            (function_definition parameters: (parameters) @params)
            (type_annotation) @typed
            (with_statement) @with
            "#,
        )
        .ok()?;

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, tree.root_node(), text.as_bytes());

        let mut typed_count = 0;
        let mut with_count = 0;

        while let Some(m) = matches.next() {
            for cap in m.captures {
                let name = query.capture_names()[cap.index as usize];
                match name {
                    "typed" => typed_count += 1,
                    "with" => with_count += 1,
                    _ => {}
                }
            }
        }

        let mut out = Vec::new();
        if typed_count > 2 {
            out.push("Use PEP 484 type hints for function signatures.".into());
        }
        if with_count > 0 {
            out.push(
                "Use context managers (`with` blocks) for resource handling (files, locks).".into(),
            );
        }

        if out.is_empty() {
            None
        } else {
            Some((out, 1))
        }
    }
}

fn detect_indent(text: &str) -> Option<String> {
    let mut tab = 0usize;
    let mut sp2 = 0usize;
    let mut sp4 = 0usize;

    for line in text.lines().take(4000) {
        if line.starts_with('\t') {
            tab += 1;
        } else if line.starts_with("    ") {
            sp4 += 1;
        } else if line.starts_with("  ") {
            sp2 += 1;
        }
    }

    if tab == 0 && sp2 == 0 && sp4 == 0 {
        return None;
    }

    if tab >= sp2 && tab >= sp4 {
        Some("Use tabs for indentation.".into())
    } else if sp4 >= sp2 {
        Some("Use 4 spaces for indentation.".into())
    } else {
        Some("Use 2 spaces for indentation.".into())
    }
}

fn detect_naming(text: &str) -> Option<String> {
    static SNAKE_RE: OnceLock<Regex> = OnceLock::new();
    static CAMEL_RE: OnceLock<Regex> = OnceLock::new();
    static PASCAL_RE: OnceLock<Regex> = OnceLock::new();

    let snake =
        SNAKE_RE.get_or_init(|| Regex::new(r"\b[a-z]+_[a-z0-9_]+\b").expect("Invalid snake regex"));
    let camel = CAMEL_RE
        .get_or_init(|| Regex::new(r"\b[a-z]+[A-Z][A-Za-z0-9]*\b").expect("Invalid camel regex"));
    let pascal = PASCAL_RE.get_or_init(|| {
        Regex::new(r"\b[A-Z][a-z0-9]+[A-Za-z0-9]*\b").expect("Invalid pascal regex")
    });

    let mut s = 0usize;
    let mut c = 0usize;
    let mut p = 0usize;

    for m in snake.find_iter(text).take(2000) {
        let _ = m;
        s += 1;
    }
    for m in camel.find_iter(text).take(2000) {
        let _ = m;
        c += 1;
    }
    for m in pascal.find_iter(text).take(2000) {
        let _ = m;
        p += 1;
    }

    let total = s + c + p;
    if total < 5 {
        return None;
    }

    let msg = if s >= c && s >= p {
        "Prefer snake_case for identifiers."
    } else if c >= s && c >= p {
        "Prefer camelCase for local identifiers."
    } else {
        "Prefer PascalCase for types, and keep naming consistent within the file."
    };

    Some(msg.into())
}

fn detect_error_handling(text: &str) -> Option<String> {
    let uses_result =
        text.contains("Result<") || text.contains("anyhow::") || text.contains("thiserror::");
    let uses_qmark = text.contains("?");
    let uses_unwrap = text.contains("unwrap(") || text.contains("expect(");

    if uses_result && uses_qmark {
        Some("Use Result-returning APIs and propagate errors with `?` (avoid deep nesting).".into())
    } else if uses_unwrap {
        Some("Avoid `unwrap()`/`expect()` in production paths unless justified; prefer explicit error handling.".into())
    } else {
        None
    }
}

fn detect_import_style(text: &str) -> Option<String> {
    if text.contains("use ") && text.contains("crate::") {
        Some(
            "Prefer explicit imports (e.g., `use crate::...`) and keep them grouped at the top."
                .into(),
        )
    } else if text.contains("from ") && text.contains("import ") {
        Some("Prefer `from X import Y` style imports and keep them grouped at the top.".into())
    } else {
        None
    }
}

fn detect_docs(text: &str) -> Option<String> {
    let rust_docs = text.contains("///") || text.contains("//! ");
    let py_docs = text.contains("\"\"\"") || text.contains("''' ");

    if rust_docs {
        Some("Add brief Rust doc comments (`///`) for public functions/types; keep them close to the item.".into())
    } else if py_docs {
        Some("Use docstrings for public functions/classes and keep them concise.".into())
    } else {
        None
    }
}

fn detect_logging(text: &str) -> Option<String> {
    let rust_logging = text.contains("info!")
        || text.contains("debug!")
        || text.contains("error!")
        || text.contains("warn!")
        || text.contains("trace!");
    let py_logging = text.contains("logging.") || text.contains("logger.");

    if rust_logging {
        Some("Use structured logging macro (e.g., `info!`, `error!`) to record significant events and errors.".into())
    } else if py_logging {
        Some("Use the `logging` module for standardized log levels and output formatting.".into())
    } else {
        None
    }
}

fn detect_testing(text: &str) -> Option<String> {
    let rust_tests =
        text.contains("#[test]") || text.contains("#[tokio::test]") || text.contains("cfg(test)");
    let py_tests = text.contains("test_") || text.contains("pytest") || text.contains("unittest");

    if rust_tests {
        Some("Include unit tests using `#[test]` or `#[tokio::test]` within a `tests` module or alongside code.".into())
    } else if py_tests {
        Some("Use `pytest` conventions with `test_` prefixes for automated verification.".into())
    } else {
        None
    }
}
