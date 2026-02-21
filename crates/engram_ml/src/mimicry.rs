use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, Query, QueryCursor};

fn get_compiled_regex<'a>(
    lock: &'a OnceLock<Regex>,
    pattern: &str,
    label: &str,
) -> Option<&'a Regex> {
    if let Some(re) = lock.get() {
        return Some(re);
    }
    match Regex::new(pattern) {
        Ok(re) => Some(lock.get_or_init(|| re)),
        Err(err) => {
            tracing::error!("failed to compile {label} regex: {err}");
            None
        }
    }
}

/// Detected language from AST analysis, used for confidence weighting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DetectedLanguage {
    Rust,
    Python,
    CSharp,
    TypeScript,
    Java,
    Go,
    Unknown,
}

impl DetectedLanguage {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::CSharp => "csharp",
            Self::TypeScript => "typescript",
            Self::Java => "java",
            Self::Go => "go",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StyleGuide {
    pub bullets: Vec<String>,
    pub confidence: f32,
    pub evidence_snippets: Vec<String>,
    /// Primary language detected via AST parsing.
    #[serde(default)]
    pub detected_language: Option<String>,
    /// Number of distinct detection categories that fired.
    #[serde(default)]
    pub detection_count: usize,
}

impl StyleGuide {
    pub fn as_text(&self) -> String {
        if self.bullets.is_empty() {
            return "INSUFFICIENT_DATA".into();
        }
        let mut out = format!("Confidence: {:.2}\n", self.confidence);
        if let Some(lang) = &self.detected_language {
            out.push_str(&format!("Language: {lang}\n"));
        }
        out.push('\n');
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

/// Weighted detection result from a single analyzer pass.
struct Detection {
    bullets: Vec<String>,
    /// Weight: how many detection "slots" this result occupies (1..3).
    weight: usize,
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
                detected_language: None,
                detection_count: 0,
            };
        }

        let mut bullets = Vec::new();
        let mut detection_weight = 0usize;

        // 1. AST-based semantic analysis (multi-language)
        let (ast_bullets, ast_weight, detected_lang) = self.semantic_analyze(&text);
        bullets.extend(ast_bullets);
        detection_weight += ast_weight;

        // 2. Lexical/Text-based detections (language-agnostic and language-specific)
        if let Some(i) = detect_indent(&text) {
            bullets.push(i);
            detection_weight += 1;
        }

        if let Some(e) = detect_error_handling(&text, detected_lang) {
            bullets.push(e);
            detection_weight += 1;
        }

        if let Some(i) = detect_import_style(&text, detected_lang) {
            bullets.push(i);
            detection_weight += 1;
        }

        if let Some(d) = detect_docs(&text, detected_lang) {
            bullets.push(d);
            detection_weight += 1;
        }

        if let Some(l) = detect_logging(&text, detected_lang) {
            bullets.push(l);
            detection_weight += 1;
        }

        if let Some(t) = detect_testing(&text, detected_lang) {
            bullets.push(t);
            detection_weight += 1;
        }

        if let Some(n) = detect_naming(&text) {
            bullets.push(n);
            detection_weight += 1;
        }

        if let Some(ll) = detect_line_length(&text) {
            bullets.push(ll);
            detection_weight += 1;
        }

        if let Some(a) = detect_async_patterns(&text, detected_lang) {
            bullets.push(a);
            detection_weight += 1;
        }

        // Confidence: weighted detection quality, scaled to 0.0-1.0.
        // AST detections carry more weight than lexical. Language-specific
        // detections are more reliable. 10 detection-weight = 1.0 confidence.
        let confidence = (detection_weight as f32 / 10.0).min(1.0);

        // Evidence window: pick a few interesting lines (non-blank, non-import)
        let mut evidence_snippets = Vec::new();
        let lines: Vec<&str> = text
            .lines()
            .filter(|l| {
                let trimmed = l.trim();
                !trimmed.is_empty()
                    && !trimmed.starts_with("use ")
                    && !trimmed.starts_with("import ")
                    && !trimmed.starts_with("using ")
                    && !trimmed.starts_with("from ")
            })
            .collect();
        if lines.len() >= 3 {
            let step = lines.len() / 3;
            if step > 0 {
                for i in (0..lines.len().saturating_sub(3)).step_by(step).take(3) {
                    let end = (i + 3).min(lines.len());
                    evidence_snippets.push(lines[i..end].join("\n"));
                }
            }
        }

        let detection_count = bullets.len();
        bullets.truncate(8);
        StyleGuide {
            bullets,
            confidence,
            evidence_snippets,
            detected_language: if detected_lang != DetectedLanguage::Unknown {
                Some(detected_lang.as_str().to_string())
            } else {
                None
            },
            detection_count,
        }
    }

    fn semantic_analyze(&self, text: &str) -> (Vec<String>, usize, DetectedLanguage) {
        let mut bullets = Vec::new();
        let mut weight = 0;
        let mut detected = DetectedLanguage::Unknown;

        // Try each language's AST analyzer; the first successful parse wins as "primary".
        // Order: Rust, C#, TypeScript, Java, Go, Python (most specific -> most generic).
        let analyzers: &[fn(&StyleMimicryEngine, &str) -> Option<(Detection, DetectedLanguage)>] =
            &[
                Self::analyze_rust,
                Self::analyze_csharp,
                Self::analyze_typescript,
                Self::analyze_java,
                Self::analyze_go,
                Self::analyze_python,
            ];

        for analyzer in analyzers {
            if let Some((det, lang)) = analyzer(self, text) {
                bullets.extend(det.bullets);
                weight += det.weight;
                if detected == DetectedLanguage::Unknown {
                    detected = lang;
                }
                // Only take the first successful AST parse to avoid mixed-language noise.
                break;
            }
        }

        (bullets, weight, detected)
    }

    fn analyze_rust(&self, text: &str) -> Option<(Detection, DetectedLanguage)> {
        if !text.contains("fn ") && !text.contains("struct ") {
            return None;
        }

        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .ok()?;
        let tree = parser.parse(text, None)?;

        if count_errors(tree.root_node()) > 5 {
            return None;
        }

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
            Some((
                Detection {
                    weight: out.len().min(3),
                    bullets: out,
                },
                DetectedLanguage::Rust,
            ))
        }
    }

    fn analyze_python(&self, text: &str) -> Option<(Detection, DetectedLanguage)> {
        if !text.contains("def ") && !text.contains("class ") {
            return None;
        }

        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .ok()?;
        let tree = parser.parse(text, None)?;

        if count_errors(tree.root_node()) > 5 {
            return None;
        }

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
            Some((
                Detection {
                    weight: out.len().min(3),
                    bullets: out,
                },
                DetectedLanguage::Python,
            ))
        }
    }

    fn analyze_csharp(&self, text: &str) -> Option<(Detection, DetectedLanguage)> {
        if !text.contains("namespace ") && !text.contains("class ") && !text.contains("void ") {
            return None;
        }
        let csharp_signals = text.contains("using System")
            || text.contains("using Microsoft")
            || text.contains("partial class")
            || text.contains("string ")
            || text.contains("var ")
            || text.contains("async Task");
        if !csharp_signals {
            return None;
        }

        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_c_sharp::LANGUAGE.into())
            .ok()?;
        let tree = parser.parse(text, None)?;

        if count_errors(tree.root_node()) > 5 {
            return None;
        }

        let query = Query::new(
            &tree_sitter_c_sharp::LANGUAGE.into(),
            r#"
            (method_declaration) @method
            (property_declaration) @property
            (using_directive) @using
            (nullable_type) @nullable
            "#,
        )
        .ok()?;

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, tree.root_node(), text.as_bytes());

        let mut method_count = 0;
        let mut property_count = 0;
        let mut using_count = 0;
        let mut nullable_count = 0;

        while let Some(m) = matches.next() {
            for cap in m.captures {
                let name = query.capture_names()[cap.index as usize];
                match name {
                    "method" => method_count += 1,
                    "property" => property_count += 1,
                    "using" => using_count += 1,
                    "nullable" => nullable_count += 1,
                    _ => {}
                }
            }
        }

        let mut out = Vec::new();
        if property_count > 0 && method_count > 0 {
            out.push("Use properties for state exposure; keep methods for behavior.".into());
        }
        if nullable_count > 2 {
            out.push("Use nullable reference types (`T?`) consistently; enable `<Nullable>enable</Nullable>`.".into());
        }
        if using_count > 5 {
            out.push(
                "Group `using` directives at the top: System, Microsoft, then project namespaces."
                    .into(),
            );
        }
        if text.contains("IDisposable") || text.contains("using (") || text.contains("using var") {
            out.push(
                "Implement `IDisposable` and use `using` declarations for resource cleanup.".into(),
            );
        }

        if out.is_empty() {
            None
        } else {
            Some((
                Detection {
                    weight: out.len().min(3),
                    bullets: out,
                },
                DetectedLanguage::CSharp,
            ))
        }
    }

    fn analyze_typescript(&self, text: &str) -> Option<(Detection, DetectedLanguage)> {
        let ts_signals = text.contains("interface ")
            || text.contains(": string")
            || text.contains(": number")
            || text.contains("<T>")
            || text.contains("readonly ");
        let js_signals = text.contains("function ")
            || text.contains("const ")
            || text.contains("=>")
            || text.contains("export ");
        if !ts_signals && !js_signals {
            return None;
        }

        let lang = tree_sitter_typescript::LANGUAGE_TYPESCRIPT;
        let mut parser = Parser::new();
        parser.set_language(&lang.into()).ok()?;
        let tree = parser.parse(text, None)?;

        if count_errors(tree.root_node()) > 5 {
            return None;
        }

        let query = Query::new(
            &lang.into(),
            r#"
            (function_declaration) @func
            (arrow_function) @arrow
            (interface_declaration) @iface
            (type_alias_declaration) @type_alias
            (lexical_declaration) @let_const
            "#,
        )
        .ok()?;

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, tree.root_node(), text.as_bytes());

        let mut func_count = 0;
        let mut arrow_count = 0;
        let mut iface_count = 0;
        let mut type_alias_count = 0;
        let mut let_const_count = 0;

        while let Some(m) = matches.next() {
            for cap in m.captures {
                let name = query.capture_names()[cap.index as usize];
                match name {
                    "func" => func_count += 1,
                    "arrow" => arrow_count += 1,
                    "iface" => iface_count += 1,
                    "type_alias" => type_alias_count += 1,
                    "let_const" => let_const_count += 1,
                    _ => {}
                }
            }
        }

        let mut out = Vec::new();
        if arrow_count > func_count && arrow_count > 2 {
            out.push(
                "Prefer arrow functions for callbacks and short expressions; use `function` for named exports."
                    .into(),
            );
        } else if func_count > arrow_count && func_count > 2 {
            out.push(
                "Use named `function` declarations for top-level exports and hoisting.".into(),
            );
        }
        if iface_count > 0 || type_alias_count > 0 {
            out.push(
                "Define explicit TypeScript interfaces/types for data shapes and API contracts."
                    .into(),
            );
        }
        if let_const_count > 3 && !text.contains("var ") {
            out.push("Use `const` by default, `let` for reassignment; never use `var`.".into());
        }

        if out.is_empty() {
            None
        } else {
            Some((
                Detection {
                    weight: out.len().min(3),
                    bullets: out,
                },
                DetectedLanguage::TypeScript,
            ))
        }
    }

    fn analyze_java(&self, text: &str) -> Option<(Detection, DetectedLanguage)> {
        if !text.contains("class ") && !text.contains("interface ") {
            return None;
        }
        let java_signals = text.contains("package ")
            || text.contains("System.out")
            || text.contains("@Override")
            || text.contains("extends ")
            || text.contains("implements ");
        if !java_signals {
            return None;
        }

        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .ok()?;
        let tree = parser.parse(text, None)?;

        if count_errors(tree.root_node()) > 5 {
            return None;
        }

        let query = Query::new(
            &tree_sitter_java::LANGUAGE.into(),
            r#"
            (method_declaration) @method
            (class_declaration) @class
            (marker_annotation name: (identifier) @annotation)
            (try_with_resources_statement) @try_resources
            "#,
        )
        .ok()?;

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, tree.root_node(), text.as_bytes());

        let mut method_count = 0;
        let mut class_count = 0;
        let mut annotation_count = 0;
        let mut try_resources_count = 0;

        while let Some(m) = matches.next() {
            for cap in m.captures {
                let name = query.capture_names()[cap.index as usize];
                match name {
                    "method" => method_count += 1,
                    "class" => class_count += 1,
                    "annotation" => annotation_count += 1,
                    "try_resources" => try_resources_count += 1,
                    _ => {}
                }
            }
        }

        let mut out = Vec::new();
        if annotation_count > 2 {
            out.push(
                "Use annotations (`@Override`, `@Nullable`, `@NonNull`) consistently for contract enforcement."
                    .into(),
            );
        }
        if try_resources_count > 0 {
            out.push("Use try-with-resources for `AutoCloseable` objects.".into());
        }
        if class_count > 0 && method_count > 10 {
            out.push(
                "Consider single-responsibility: extract classes when method count grows large."
                    .into(),
            );
        }
        if text.contains("Optional<") || text.contains("Optional.of") {
            out.push(
                "Use `Optional<T>` for nullable return types instead of returning null.".into(),
            );
        }

        if out.is_empty() {
            None
        } else {
            Some((
                Detection {
                    weight: out.len().min(3),
                    bullets: out,
                },
                DetectedLanguage::Java,
            ))
        }
    }

    fn analyze_go(&self, text: &str) -> Option<(Detection, DetectedLanguage)> {
        if !text.contains("func ") && !text.contains("package ") {
            return None;
        }
        let go_signals = text.contains("package main")
            || text.contains("package ")
            || text.contains(":=")
            || text.contains("func (")
            || text.contains("go func");
        if !go_signals {
            return None;
        }

        let mut parser = Parser::new();
        parser.set_language(&tree_sitter_go::LANGUAGE.into()).ok()?;
        let tree = parser.parse(text, None)?;

        if count_errors(tree.root_node()) > 5 {
            return None;
        }

        let query = Query::new(
            &tree_sitter_go::LANGUAGE.into(),
            r#"
            (function_declaration) @func
            (method_declaration) @method
            (if_statement consequence: (block) @if_body)
            (defer_statement) @defer
            "#,
        )
        .ok()?;

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, tree.root_node(), text.as_bytes());

        let mut func_count = 0;
        let mut method_count = 0;
        let mut defer_count = 0;

        while let Some(m) = matches.next() {
            for cap in m.captures {
                let name = query.capture_names()[cap.index as usize];
                match name {
                    "func" => func_count += 1,
                    "method" => method_count += 1,
                    "defer" => defer_count += 1,
                    _ => {}
                }
            }
        }

        let mut out = Vec::new();
        if text.contains("if err != nil") {
            out.push(
                "Follow idiomatic error handling: `if err != nil { return ..., err }` early-return style."
                    .into(),
            );
        }
        if defer_count > 0 {
            out.push("Use `defer` for cleanup (closing files, releasing locks).".into());
        }
        if method_count > 0 && func_count > 0 {
            out.push(
                "Use receiver methods for type behavior; use standalone functions for utilities."
                    .into(),
            );
        }
        if text.contains("chan ") || text.contains("go func") {
            out.push(
                "Use goroutines and channels for concurrency; avoid shared mutable state.".into(),
            );
        }

        if out.is_empty() {
            None
        } else {
            Some((
                Detection {
                    weight: out.len().min(3),
                    bullets: out,
                },
                DetectedLanguage::Go,
            ))
        }
    }
}

/// Count ERROR nodes in a parse tree to detect wrong-language parsing.
fn count_errors(node: tree_sitter::Node<'_>) -> usize {
    let mut count = if node.is_error() || node.is_missing() {
        1
    } else {
        0
    };
    let child_count = node.child_count();
    for i in 0..child_count {
        if let Some(child) = node.child(i) {
            count += count_errors(child);
        }
        if count > 5 {
            return count;
        }
    }
    count
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

    let Some(snake) = get_compiled_regex(&SNAKE_RE, r"\b[a-z]+_[a-z0-9_]+\b", "mimicry_snake")
    else {
        return None;
    };
    let Some(camel) =
        get_compiled_regex(&CAMEL_RE, r"\b[a-z]+[A-Z][A-Za-z0-9]*\b", "mimicry_camel")
    else {
        return None;
    };
    let Some(pascal) = get_compiled_regex(
        &PASCAL_RE,
        r"\b[A-Z][a-z0-9]+[A-Za-z0-9]*\b",
        "mimicry_pascal",
    ) else {
        return None;
    };

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

fn detect_error_handling(text: &str, lang: DetectedLanguage) -> Option<String> {
    match lang {
        DetectedLanguage::Rust => {
            let uses_result = text.contains("Result<")
                || text.contains("anyhow::")
                || text.contains("thiserror::");
            let uses_qmark = text.contains('?');
            let uses_unwrap = text.contains("unwrap(") || text.contains("expect(");
            if uses_result && uses_qmark {
                Some(
                    "Use Result-returning APIs and propagate errors with `?` (avoid deep nesting)."
                        .into(),
                )
            } else if uses_unwrap {
                Some("Avoid `unwrap()`/`expect()` in production paths unless justified; prefer explicit error handling.".into())
            } else {
                None
            }
        }
        DetectedLanguage::Go => {
            if text.contains("panic(") {
                Some("Avoid `panic()` in library code; return errors to callers.".into())
            } else {
                None
            }
        }
        DetectedLanguage::CSharp | DetectedLanguage::Java => {
            let has_catch_all = text.contains("catch (Exception")
                || text.contains("catch (System.Exception")
                || text.contains("catch {");
            if has_catch_all {
                Some("Avoid catching generic `Exception`; catch specific exception types.".into())
            } else if text.contains("try {") || text.contains("try\n") {
                Some("Use structured exception handling with specific catch blocks.".into())
            } else {
                None
            }
        }
        DetectedLanguage::TypeScript => {
            let has_try = text.contains("try {");
            let has_catch_unknown = text.contains("catch (e)") || text.contains("catch (error)");
            if has_try && has_catch_unknown {
                Some(
                    "Type-narrow caught exceptions; use `instanceof` checks in catch blocks."
                        .into(),
                )
            } else {
                None
            }
        }
        DetectedLanguage::Python => {
            if text.contains("except:") {
                Some("Avoid bare `except:`; catch specific exception classes.".into())
            } else {
                None
            }
        }
        DetectedLanguage::Unknown => {
            let uses_result = text.contains("Result<")
                || text.contains("anyhow::")
                || text.contains("thiserror::");
            let uses_qmark = text.contains('?');
            let uses_unwrap = text.contains("unwrap(") || text.contains("expect(");
            if uses_result && uses_qmark {
                Some(
                    "Use Result-returning APIs and propagate errors with `?` (avoid deep nesting)."
                        .into(),
                )
            } else if uses_unwrap {
                Some("Avoid `unwrap()`/`expect()` in production paths; prefer explicit error handling.".into())
            } else {
                None
            }
        }
    }
}

fn detect_import_style(text: &str, lang: DetectedLanguage) -> Option<String> {
    match lang {
        DetectedLanguage::Rust => {
            if text.contains("use ") && text.contains("crate::") {
                Some("Prefer explicit imports (e.g., `use crate::...`) and keep them grouped at the top.".into())
            } else {
                None
            }
        }
        DetectedLanguage::Python => {
            if text.contains("from ") && text.contains("import ") {
                Some(
                    "Prefer `from X import Y` style imports and keep them grouped at the top."
                        .into(),
                )
            } else {
                None
            }
        }
        DetectedLanguage::CSharp => {
            if text.contains("using ") {
                Some("Group `using` directives at the top: System, Microsoft, then project namespaces.".into())
            } else {
                None
            }
        }
        DetectedLanguage::TypeScript => {
            let has_import = text.contains("import ");
            let has_require = text.contains("require(");
            if has_import && !has_require {
                Some("Use ES module `import` syntax; avoid CommonJS `require()`.".into())
            } else if has_require && !has_import {
                Some(
                    "Use CommonJS `require()` consistently; consider migrating to ES modules."
                        .into(),
                )
            } else {
                None
            }
        }
        DetectedLanguage::Java => {
            if text.contains("import ") {
                if text.contains("import java.*") || text.contains(".*;\n") {
                    Some("Avoid wildcard imports (`import x.*`); import specific classes.".into())
                } else {
                    Some(
                        "Keep imports organized: java/javax first, then third-party, then project."
                            .into(),
                    )
                }
            } else {
                None
            }
        }
        DetectedLanguage::Go => {
            if text.contains("import (") {
                Some(
                    "Group imports: stdlib, blank line, third-party, blank line, project packages."
                        .into(),
                )
            } else {
                None
            }
        }
        DetectedLanguage::Unknown => {
            if text.contains("use ") && text.contains("crate::") {
                Some("Prefer explicit imports and keep them grouped at the top.".into())
            } else if text.contains("from ") && text.contains("import ") {
                Some(
                    "Prefer `from X import Y` style imports and keep them grouped at the top."
                        .into(),
                )
            } else {
                None
            }
        }
    }
}

fn detect_docs(text: &str, lang: DetectedLanguage) -> Option<String> {
    match lang {
        DetectedLanguage::Rust => {
            if text.contains("///") || text.contains("//! ") {
                Some("Add brief Rust doc comments (`///`) for public functions/types; keep them close to the item.".into())
            } else {
                None
            }
        }
        DetectedLanguage::Python => {
            if text.contains("\"\"\"") || text.contains("''' ") {
                Some("Use docstrings for public functions/classes and keep them concise.".into())
            } else {
                None
            }
        }
        DetectedLanguage::CSharp => {
            if text.contains("/// <summary>") || text.contains("/// <param") {
                Some("Use XML doc comments (`/// <summary>`) for public API documentation.".into())
            } else {
                None
            }
        }
        DetectedLanguage::Java => {
            if text.contains("/**") && text.contains("*/") {
                Some("Use Javadoc (`/** */`) for public API documentation with `@param` and `@return` tags.".into())
            } else {
                None
            }
        }
        DetectedLanguage::TypeScript => {
            if text.contains("/**") || text.contains("* @param") {
                Some("Use JSDoc (`/** */`) for function documentation; keep comments brief.".into())
            } else {
                None
            }
        }
        DetectedLanguage::Go => {
            if text.contains("// ") {
                Some("Start doc comments with the function/type name (Go convention).".into())
            } else {
                None
            }
        }
        DetectedLanguage::Unknown => {
            if text.contains("///") || text.contains("//! ") {
                Some("Add doc comments for public functions/types.".into())
            } else if text.contains("\"\"\"") || text.contains("''' ") {
                Some("Use docstrings for public functions/classes.".into())
            } else {
                None
            }
        }
    }
}

fn detect_logging(text: &str, lang: DetectedLanguage) -> Option<String> {
    match lang {
        DetectedLanguage::Rust => {
            let tracing = text.contains("info!")
                || text.contains("debug!")
                || text.contains("error!")
                || text.contains("warn!")
                || text.contains("trace!");
            if tracing {
                Some("Use structured logging macro (e.g., `info!`, `error!`) to record significant events and errors.".into())
            } else {
                None
            }
        }
        DetectedLanguage::Python => {
            if text.contains("logging.") || text.contains("logger.") {
                Some(
                    "Use the `logging` module for standardized log levels and output formatting."
                        .into(),
                )
            } else {
                None
            }
        }
        DetectedLanguage::CSharp => {
            if text.contains("ILogger") || text.contains("_logger.") {
                Some("Use `ILogger<T>` with structured logging; avoid `Console.WriteLine` for diagnostics.".into())
            } else {
                None
            }
        }
        DetectedLanguage::Java => {
            if text.contains("Logger") || text.contains("log.") || text.contains("LOG.") {
                Some("Use SLF4J/Log4j for structured logging; avoid `System.out.println`.".into())
            } else {
                None
            }
        }
        DetectedLanguage::TypeScript => {
            if text.contains("console.log") || text.contains("console.error") {
                Some("Replace `console.log` with a structured logger (e.g., winston, pino) in production.".into())
            } else {
                None
            }
        }
        DetectedLanguage::Go => {
            if text.contains("log.") || text.contains("slog.") || text.contains("zap.") {
                Some("Use structured logging (slog, zap) with context fields; avoid bare `fmt.Println`.".into())
            } else {
                None
            }
        }
        DetectedLanguage::Unknown => {
            let rust_logging = text.contains("info!")
                || text.contains("debug!")
                || text.contains("error!")
                || text.contains("warn!")
                || text.contains("trace!");
            if rust_logging {
                Some("Use structured logging macros for recording events and errors.".into())
            } else if text.contains("logging.") || text.contains("logger.") {
                Some("Use the `logging` module for standardized log levels.".into())
            } else {
                None
            }
        }
    }
}

fn detect_testing(text: &str, lang: DetectedLanguage) -> Option<String> {
    match lang {
        DetectedLanguage::Rust => {
            if text.contains("#[test]")
                || text.contains("#[tokio::test]")
                || text.contains("cfg(test)")
            {
                Some("Include unit tests using `#[test]` or `#[tokio::test]` within a `tests` module or alongside code.".into())
            } else {
                None
            }
        }
        DetectedLanguage::Python => {
            if text.contains("test_") || text.contains("pytest") || text.contains("unittest") {
                Some(
                    "Use `pytest` conventions with `test_` prefixes for automated verification."
                        .into(),
                )
            } else {
                None
            }
        }
        DetectedLanguage::CSharp => {
            if text.contains("[Fact]")
                || text.contains("[Theory]")
                || text.contains("[Test]")
                || text.contains("[TestMethod]")
            {
                Some("Use xUnit/NUnit test attributes (`[Fact]`, `[Theory]`, `[Test]`) with descriptive method names.".into())
            } else {
                None
            }
        }
        DetectedLanguage::Java => {
            if text.contains("@Test") || text.contains("@ParameterizedTest") {
                Some("Use JUnit annotations (`@Test`, `@ParameterizedTest`) with Arrange-Act-Assert structure.".into())
            } else {
                None
            }
        }
        DetectedLanguage::TypeScript => {
            if text.contains("describe(")
                || text.contains("it(")
                || text.contains("test(")
                || text.contains("expect(")
            {
                Some("Use describe/it blocks with clear expectations; prefer integration tests for API boundaries.".into())
            } else {
                None
            }
        }
        DetectedLanguage::Go => {
            if text.contains("func Test") || text.contains("testing.T") {
                Some("Use table-driven tests (`func TestX(t *testing.T)`) with descriptive subtests.".into())
            } else {
                None
            }
        }
        DetectedLanguage::Unknown => {
            if text.contains("#[test]")
                || text.contains("#[tokio::test]")
                || text.contains("cfg(test)")
            {
                Some("Include unit tests using test attributes or test modules.".into())
            } else if text.contains("test_") || text.contains("pytest") || text.contains("unittest")
            {
                Some("Use `pytest` conventions with `test_` prefixes.".into())
            } else {
                None
            }
        }
    }
}

fn detect_line_length(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().take(2000).collect();
    if lines.len() < 20 {
        return None;
    }

    let mut long_count = 0usize;
    let mut total_len = 0usize;
    for line in &lines {
        let len = line.len();
        total_len += len;
        if len > 120 {
            long_count += 1;
        }
    }

    let avg = total_len / lines.len();
    let long_pct = (long_count * 100) / lines.len();

    if long_pct > 30 {
        Some(format!(
            "High ratio of long lines ({long_pct}% > 120 chars, avg {avg}); consider wrapping at 100-120 columns."
        ))
    } else if avg < 40 && lines.len() > 50 {
        Some("Lines are consistently short; this is good for readability.".into())
    } else {
        None
    }
}

fn detect_async_patterns(text: &str, lang: DetectedLanguage) -> Option<String> {
    match lang {
        DetectedLanguage::Rust => {
            if text.contains("async fn") || text.contains(".await") {
                Some("Use `async fn` + `.await` for non-blocking I/O; wrap blocking code in `spawn_blocking`.".into())
            } else {
                None
            }
        }
        DetectedLanguage::CSharp => {
            if text.contains("async Task") || text.contains("await ") {
                Some("Use `async`/`await` for I/O-bound operations; suffix async methods with `Async`.".into())
            } else {
                None
            }
        }
        DetectedLanguage::TypeScript => {
            let has_async = text.contains("async ") || text.contains("await ");
            let has_promise = text.contains("Promise<") || text.contains(".then(");
            if has_async {
                Some("Use `async`/`await` over raw Promise chains for readability.".into())
            } else if has_promise {
                Some("Consider converting Promise chains to `async`/`await` for clarity.".into())
            } else {
                None
            }
        }
        DetectedLanguage::Python => {
            if text.contains("async def") || text.contains("await ") {
                Some("Use `async def` + `await` for I/O-bound coroutines; use `asyncio.run` as entry point.".into())
            } else {
                None
            }
        }
        DetectedLanguage::Java => {
            if text.contains("CompletableFuture") || text.contains("ExecutorService") {
                Some("Use `CompletableFuture` for async composition; prefer virtual threads (Java 21+) for blocking I/O.".into())
            } else {
                None
            }
        }
        _ => None,
    }
}
