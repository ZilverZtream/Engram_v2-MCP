//! MiniLang (`.ml`, `.mlinc`) extractor.
//!
//! MiniLang is a native systems language with VB-flavoured, line-oriented,
//! block-structured syntax: blocks open with a keyword and close with
//! `End <keyword>`. No tree-sitter grammar exists, so this is a hand-rolled
//! line scanner in the same spirit as `asp_classic_extractor`.
//!
//! The grammar implemented here was derived from the MiniLang corpus, NOT
//! from `LANGUAGE.md`, which omits `Sub`, access modifiers, `Throws`
//! clauses, and generic constraints — all widespread in the shipped
//! standard library.

pub mod bodies;
pub mod decls;
pub mod ui;

use crate::parsing::{ExtractedEdge, ExtractedSymbol};
use std::path::Path;

/// Block keywords that open a nesting level. Every one is closed by
/// `End <keyword>`. Control-flow blocks (`If`, `While`, `Try`, …) emit no
/// symbols but MUST be tracked, otherwise their `End` lines would close the
/// enclosing function early.
pub(crate) const BLOCK_KEYWORDS: &[&str] = &[
    // Declaration blocks — these produce symbols.
    "Namespace",
    "Function",
    "Sub",
    "Type",
    "Enum",
    "Interface",
    // Control flow and scoping — tracked for balance only.
    "If",
    "While",
    "Try",
    "Match",
    "Select",
    "Switch",
    "SelectChannel",
    "Unsafe",
    "Using",
    "Asm",
    // UI DSL — handled by `ui.rs`.
    "Ui",
    "Panel",
    "Label",
    "Button",
    "Badge",
    "Card",
    "Field",
    "Checkbox",
    "Radio",
    "Switch2",
    "Slider",
    "ProgressBar",
    "Image",
    "Divider",
    "VStack",
];

/// Strip a line comment, respecting double-quoted string literals.
///
/// MiniLang accepts `'`, `#`, and `//` as comment markers. It has NO
/// character-literal syntax (`Char` values come from
/// `Std.Convert.IntToChar`), so a bare `'` outside a string is always a
/// comment — there is no ambiguity to resolve.
pub(crate) fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut in_string = false;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            if b == b'\\' {
                // Escape sequence: skip the escaped byte.
                i += 2;
                continue;
            }
            if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'\'' | b'#' => return &line[..i],
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => return &line[..i],
            _ => {}
        }
        i += 1;
    }
    line
}

/// The block keyword a line opens, if any.
///
/// Skips optional `Public`/`Private` access modifiers. Anchors on the
/// keyword being the FIRST significant token: this is what keeps the type
/// annotation `Mapper As Function(T) As R` from registering as a
/// declaration.
pub(crate) fn block_opener(trimmed: &str) -> Option<&'static str> {
    let mut rest = trimmed;
    for modifier in ["Public ", "Private "] {
        if let Some(r) = rest.strip_prefix(modifier) {
            rest = r.trim_start();
        }
    }
    // `Declare Function` / `Extern "C" Function` are single-line bindings,
    // not blocks — they must not open a nesting level.
    if rest.starts_with("Declare ") || rest.starts_with("Extern ") {
        return None;
    }
    for kw in BLOCK_KEYWORDS {
        // `Unsafe(RawPtr)` has no space before its capability list, so match
        // the keyword followed by a space, an open paren, or end-of-line.
        if let Some(after) = rest.strip_prefix(*kw) {
            if after.is_empty() || after.starts_with(' ') || after.starts_with('(') {
                return Some(kw);
            }
        }
    }
    None
}

/// The keyword an `End X` line closes, if any.
pub(crate) fn block_closer(trimmed: &str) -> Option<String> {
    let rest = trimmed.strip_prefix("End")?;
    let rest = rest.strip_prefix(' ')?.trim();
    if rest.is_empty() {
        return None;
    }
    Some(rest.to_string())
}

/// One open block on the scanner's stack.
pub(crate) struct OpenBlock {
    pub keyword: String,
    /// FQN for declaration blocks; empty for control-flow blocks.
    pub fqn: String,
    /// 1-based line where the block opened. Not yet read — reserved for
    /// diagnostics in a later task.
    #[allow(dead_code)]
    pub start_line: u32,
    /// Index into the symbol vector for the symbol this block produced,
    /// so the scanner can backfill `end_line` when the block closes.
    pub symbol_idx: Option<usize>,
    /// Locals declared directly in this block, for ownership metadata.
    pub immutable_locals: Vec<String>,
    pub mutable_locals: Vec<String>,
    /// True once a `Catch` line is seen inside this block.
    pub has_catch: bool,
}

/// Extract symbols and edges from a MiniLang source file.
///
/// `abs_path` is the absolute disk path — used only to stat conformance-test
/// golden siblings. `rel_path` is the PROJECT-RELATIVE path and is what all
/// emitted edge targets are built from; absolute edge targets are rejected
/// by the ingest safety check.
pub fn extract_ml(
    abs_path: &Path,
    rel_path: &str,
    source: &str,
) -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>) {
    let _ = abs_path; // used from Task 7 onward for golden pairing
    let mut symbols: Vec<ExtractedSymbol> = Vec::new();
    let mut edges: Vec<ExtractedEdge> = Vec::new();
    let mut stack: Vec<OpenBlock> = Vec::new();
    let mut top_level_lines: Vec<(u32, String)> = Vec::new();

    for (idx, raw_line) in source.lines().enumerate() {
        let line_no = (idx + 1) as u32;
        let trimmed = strip_comment(raw_line).trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some(target) = decls::include_target(trimmed, rel_path) {
            edges.push(ExtractedEdge {
                source_name: rel_path.to_string(),
                source_kind: "file".to_string(),
                source_start_line: line_no,
                source_language: "ml".to_string(),
                target_name: target,
                target_kind: Some("file".to_string()),
                target_start_line: None,
                kind: "includes_file".to_string(),
                metadata: None,
            });
            continue;
        }

        if let Some(sym) = decls::parse_ffi_binding(trimmed, line_no) {
            symbols.push(sym);
            continue;
        }

        // A `Const` directly inside a Function/Sub body (real corpus case:
        // `test_mlh2380_const_ctfe.ml`'s `Const LOCAL = 6 * 7`) is local to
        // that function's execution, not a project-level declaration. Skip
        // extraction entirely rather than attributing it to whatever named
        // scope encloses the function.
        let inside_executable_scope = matches!(
            stack.last().map(|b| b.keyword.as_str()),
            Some("Function") | Some("Sub")
        );
        if !inside_executable_scope {
            let parent_fqn = stack
                .iter()
                .rev()
                .find(|b| !b.fqn.is_empty())
                .map(|b| b.fqn.as_str())
                .unwrap_or("");
            if let Some(sym) = decls::parse_const(trimmed, parent_fqn, line_no) {
                symbols.push(sym);
                continue;
            }
        }

        // Closing lines first: `End Function` also starts with a keyword
        // that would otherwise be scanned as an opener.
        if let Some(closed) = block_closer(trimmed) {
            if let Some(open) = stack.pop() {
                if let Some(i) = open.symbol_idx {
                    symbols[i].end_line = line_no;
                    if open.keyword == "Function" || open.keyword == "Sub" {
                        let mut m = symbols[i].metadata.take().unwrap_or_default();
                        if !open.immutable_locals.is_empty() {
                            m.insert("immutable_locals".into(), open.immutable_locals.join("||"));
                        }
                        if !open.mutable_locals.is_empty() {
                            m.insert("mutable_locals".into(), open.mutable_locals.join("||"));
                        }
                        if open.has_catch {
                            m.insert("has_catch".into(), "true".into());
                        }
                        if !m.is_empty() {
                            symbols[i].metadata = Some(m);
                        }
                    }
                }
                debug_assert_eq!(
                    open.keyword, closed,
                    "MiniLang block mismatch at {rel_path}:{line_no}"
                );
            }
            continue;
        }

        let opener = block_opener(trimmed);

        // Statement lines (everything that opens no block) contribute call
        // edges attributed to the innermost enclosing function. Rows
        // directly inside a Type/Enum/Interface body (fields, variants,
        // members — already classified from `body` in `open_declaration`)
        // are NOT statements and must be skipped here, exactly as they were
        // before this scanner existed: without this guard, a field row like
        // `Mapper As Function(T) As R` has no block opener either, so it
        // would fall into this branch, find no enclosing Function/Sub, and
        // be misfiled as a top-level program statement — fabricating a
        // `<module>` entry symbol in files that declare no such thing.
        if opener.is_none() {
            let in_declaration_body = matches!(
                stack.last().map(|b| b.keyword.as_str()),
                Some("Type") | Some("Enum") | Some("Interface")
            );
            if in_declaration_body {
                // A field/variant/member row: not a statement, not a
                // declaration opener. Already accounted for by `body` in
                // `open_declaration`; nothing more to do with it here.
                continue;
            }

            // This needs a mutable borrow of the stack to record locals and
            // catch regions, so it is a standalone block, resolved and
            // dropped BEFORE the immutable borrow below clones the
            // enclosing FQN — the two borrows cannot coexist.
            if let Some(owner) = stack
                .iter_mut()
                .rev()
                .find(|b| b.keyword == "Function" || b.keyword == "Sub")
            {
                if let Some((name, mutable)) = bodies::local_binding(trimmed) {
                    if mutable {
                        owner.mutable_locals.push(name);
                    } else {
                        owner.immutable_locals.push(name);
                    }
                }
                if trimmed == "Catch" || trimmed.starts_with("Catch ") {
                    owner.has_catch = true;
                }
            }

            let enclosing = stack
                .iter()
                .rev()
                .find(|b| b.keyword == "Function" || b.keyword == "Sub")
                .map(|b| b.fqn.clone());
            match enclosing {
                Some(fqn) => bodies::scan_statement(trimmed, &fqn, line_no, &mut edges),
                None => {
                    // Top level: record for the synthetic module entry.
                    top_level_lines.push((line_no, trimmed.to_string()));
                }
            }
            continue;
        }

        let Some(keyword) = opener else {
            continue;
        };

        if keyword == "Unsafe" {
            if let Some(caps) = bodies::unsafe_capabilities(trimmed) {
                if let Some(owner) = stack
                    .iter()
                    .rev()
                    .find(|b| b.keyword == "Function" || b.keyword == "Sub")
                {
                    edges.push(ExtractedEdge {
                        source_name: owner.fqn.clone(),
                        source_kind: "function".to_string(),
                        source_start_line: owner.start_line,
                        source_language: "ml".to_string(),
                        target_name: trimmed.to_string(),
                        target_kind: Some("capability".to_string()),
                        target_start_line: None,
                        kind: "dependency".to_string(),
                        metadata: decls::meta(&[
                            ("relation", "capability".to_string()),
                            ("capabilities", caps.join("||")),
                        ]),
                    });
                }
            }
        }

        let parent_fqn = stack
            .iter()
            .rev()
            .find(|b| !b.fqn.is_empty())
            .map(|b| b.fqn.as_str())
            .unwrap_or("");

        // Type/Enum/Interface bodies are classified from their rows, so
        // collect the block's lines up to its matching `End`.
        let body: Vec<&str> = if matches!(keyword, "Type" | "Enum" | "Interface") {
            collect_block_body(source, idx, keyword)
        } else {
            Vec::new()
        };

        let (fqn, symbol_idx) = decls::open_declaration(
            keyword,
            trimmed,
            parent_fqn,
            line_no,
            &body,
            &mut symbols,
            &mut edges,
        );

        stack.push(OpenBlock {
            keyword: keyword.to_string(),
            fqn,
            start_line: line_no,
            symbol_idx,
            immutable_locals: Vec::new(),
            mutable_locals: Vec::new(),
            has_catch: false,
        });
    }

    // Unterminated blocks (truncated or malformed file): close them at EOF
    // rather than dropping the symbols entirely.
    let last_line = source.lines().count() as u32;
    for open in stack {
        if let Some(i) = open.symbol_idx {
            if symbols[i].end_line == 0 {
                symbols[i].end_line = last_line;
            }
        }
    }

    // Script-style files run their top-level statements as the program
    // entry point. Give those statements a caller so their call edges are
    // not dangling. Pure-declaration files (the stdlib) get nothing.
    if !top_level_lines.is_empty() {
        let stem = rel_path
            .rsplit(['/', '\\'])
            .next()
            .and_then(|f| f.split('.').next())
            .unwrap_or("module");
        let module_fqn = format!("{stem}.<module>");
        let first = top_level_lines.first().map(|(l, _)| *l).unwrap_or(1);
        let last = top_level_lines.last().map(|(l, _)| *l).unwrap_or(first);
        for (line_no, text) in &top_level_lines {
            bodies::scan_statement(text, &module_fqn, *line_no, &mut edges);
        }
        symbols.push(ExtractedSymbol {
            name: module_fqn,
            kind: "function".to_string(),
            start_line: first,
            end_line: last,
            metadata: decls::meta(&[("synthetic", "module_entry".to_string())]),
        });
    }

    (symbols, edges)
}

/// Lines strictly inside the block that opens at `open_idx`, up to its
/// matching `End <keyword>`. Nested blocks of the same keyword are balanced.
pub(crate) fn collect_block_body<'a>(
    source: &'a str,
    open_idx: usize,
    keyword: &str,
) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut depth = 1usize;
    for line in source.lines().skip(open_idx + 1) {
        let trimmed = strip_comment(line).trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(closed) = block_closer(trimmed) {
            if closed == keyword {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            continue;
        }
        if block_opener(trimmed) == Some(keyword) {
            depth += 1;
        }
        out.push(trimmed);
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // `strip_comment` is `pub(crate)`, so its escape-handling contract is
    // pinned here directly rather than through `extract_ml`. This is
    // deliberate, not an oversight: `extract_ml` only ever inspects a
    // line's FIRST token (via `block_opener`/`block_closer`), and
    // `strip_comment` only ever truncates a line's SUFFIX, never its
    // prefix — so nothing past the declaration keyword and name can
    // change which symbols come out the other end. An escape-handling
    // regression here is real but invisible to any test built on
    // `extract_ml`'s symbol/edge output; it can only be caught here.

    #[test]
    fn strip_comment_does_not_close_string_on_escaped_quote() {
        // The escaped quote (`\"`) must not be treated as the string's
        // closing quote. If it were, the parser would exit "in string"
        // mode one character early, see the following `'` as outside any
        // string, and wrongly truncate — even though the string is not
        // actually closed until the final `"`.
        let line = "Return \"a\\\" ' not a comment\"";
        assert_eq!(strip_comment(line), line);
    }

    #[test]
    fn strip_comment_still_truncates_a_real_trailing_comment() {
        // Once the escaped quote is correctly skipped and the string
        // closes at its real final quote, a genuine trailing comment
        // (outside any string) must still be stripped. Pairs with the
        // test above so this file can't pass by simply never truncating.
        let line = "Return \"a\\\" b\" ' real comment";
        assert_eq!(strip_comment(line), "Return \"a\\\" b\" ");
    }
}
