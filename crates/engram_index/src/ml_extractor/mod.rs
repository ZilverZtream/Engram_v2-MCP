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
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Block keywords that open a nesting level. Every one is closed by
/// `End <keyword>`, EXCEPT `For`, whose dominant real-corpus closer is the
/// bare word `Next` (see `closes_block`; `End For` also works, but is a
/// construct the compiler itself rejects). Control-flow blocks (`If`,
/// `While`, `Try`, …) emit no symbols but MUST be tracked, otherwise their
/// `End` lines would close the enclosing function early.
pub(crate) const BLOCK_KEYWORDS: &[&str] = &[
    // Declaration blocks — these produce symbols.
    "Namespace",
    "Function",
    // `Func Name(...) -> Type ... End Func` — MiniLang's alternate function
    // declaration syntax (7 corpus occurrences, all in `tests/drafts/`,
    // e.g. `tests/drafts/seh_phase5_test.ml`). See `is_function_like`.
    "Func",
    "Sub",
    "Type",
    // `Union Name ... End Union` — MiniLang's EXPLICIT tagged-union
    // declaration (14 corpus files, e.g.
    // `tests/conformance/unions/test_mlh2080_nested_variant_constructors.ml`),
    // distinct from the implicit `Type`-with-variant-rows fallback the
    // `Type` arm already supports. See `decls::open_declaration`'s `Union`
    // arm.
    "Union",
    "Enum",
    "Interface",
    // Control flow and scoping — tracked for balance only.
    "If",
    "While",
    // `For i = 0 To n` / `For Each x In xs` — closed by the bare word
    // `Next` (see `closes_block`), or, in a handful of (compiler-rejected)
    // negative/fuzz fixtures, by `End For`.
    "For",
    // `Repeat N Times ... End Repeat` (45 corpus occurrences).
    "Repeat",
    "Try",
    "Match",
    "Select",
    "SelectChannel",
    "Unsafe",
    "Using",
    "Asm",
    // UI DSL — see `ui::UI_ELEMENTS`. `Switch` lives here (not in the
    // control-flow section above) because MiniLang has no control-flow
    // `Switch`/`Case`/`End Switch` construct at all — every real `Switch`
    // in the corpus is this toggle control; see `ui::UI_ELEMENTS`'s doc
    // comment for the corpus/parser evidence.
    "Ui",
    "Panel",
    "Label",
    "Button",
    "Badge",
    "Card",
    "Field",
    "Checkbox",
    "Radio",
    "Slider",
    "ProgressBar",
    "Image",
    "Divider",
    "VStack",
    "Switch",
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
    // `Parallel For …` / `Parallel For Each …` is MiniLang's explicit MIMD
    // parallel-loop form (real corpus: 60 occurrences across 9 files under
    // `tests/conformance/optimizer/`, e.g.
    // `test_mlh2270_parallel_for_syntax.ml`'s `Parallel For index = 0 To
    // 511`), closed by the same bare `Next` as an ordinary `For`.
    // `Parallel` precedes ONLY `For` in the real corpus (verified: no
    // `Parallel While`/`Parallel Repeat`/etc. exist), so this is a
    // targeted strip, not a generic modifier alongside `Public`/`Private`.
    // Without it, `Parallel For …` is not recognized as an opener at all,
    // so nothing is pushed for it — but its `Next` still closes a `For`
    // frame (see `closes_block`), wrongly popping whatever real frame (a
    // `Function`, most often) happens to be on top of the stack instead.
    if let Some(after) = rest.strip_prefix("Parallel ") {
        rest = after;
    }
    // `Declare Function` / `Extern "C" Function` are single-line bindings,
    // not blocks — they must not open a nesting level.
    if rest.starts_with("Declare ") || rest.starts_with("Extern ") {
        return None;
    }
    // `Define Style <name>` opens a reusable style-property bundle
    // (`MinSize`/`Margin`/`Padding`/`ZIndex`/… rows), closed by `End
    // Style` — a real corpus construct (8 files under
    // `tests/conformance/ui/` and `tests/drafts/`, the "mcss" styling
    // feature). It is deliberately NOT a bare `Style` entry in
    // `BLOCK_KEYWORDS`: bare `Style <name>` alone is the much more common
    // UI ATTRIBUTE row (30 occurrences, folded by `ui::ui_attribute`) —
    // registering bare `Style` as a block keyword would misparse every
    // one of those as an unterminated block. This is tracked here for
    // balance ONLY (like `If`/`While`/`Try`), so a `Define Style … End
    // Style` bundle nested inside a `Ui` block does not desync the
    // scanner's stack and corrupt every UI element after it; this task
    // does not extract the bundle's own properties.
    if let Some(after) = rest.strip_prefix("Define Style") {
        if after.is_empty() || after.starts_with(' ') {
            return Some("Style");
        }
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

/// The keyword a closing line closes, covering BOTH of MiniLang's `For`
/// loop terminator styles: the generic `End X` form (delegated to
/// `block_closer`) and the bare word `Next`, which closes a `For` frame
/// with no `End` prefix at all.
///
/// `Next` is MiniLang's overwhelmingly dominant loop terminator (thousands
/// of corpus occurrences, e.g. `benchmarks/bench_loop_sum.ml`'s `For i = 1
/// To 1000 ... Next`); `End For` is real corpus TEXT but is a construct the
/// compiler itself REJECTS (3 occurrences, all under
/// `tests/negative/syntax/` and `tests/fuzz/` — e.g. `test_end_for.ml`'s
/// own comment: "Developer writes End For instead of Next"). This scanner
/// is a lenient text scanner over real files on disk, not a validating
/// parser, so both terminator spellings must close a `For` frame — the
/// exact match against the bare literal `"Next"` (not a prefix check)
/// keeps this from colliding with the real corpus field row shape `Next As
/// <Type>` (a linked-list node's `Next` pointer, e.g.
/// `Std.Collections.Map.Core.ml`'s `Next As Int`), which is never equal to
/// the trimmed line `"Next"` alone.
pub(crate) fn closes_block(trimmed: &str) -> Option<String> {
    block_closer(trimmed).or_else(|| (trimmed == "Next").then(|| "For".to_string()))
}

/// True for a block keyword that behaves like a function body: it opens a
/// callable scope that local bindings, `Catch` regions, capability edges,
/// and call/statement attribution all key off of, and is closed by `End
/// <keyword>`.
///
/// `Func` (`Func Name(...) -> Type ... End Func`, MiniLang's alternate
/// function-declaration syntax) is semantically identical to
/// `Function`/`Sub` in every one of these respects, so everywhere this
/// scanner treats a `Function`/`Sub` frame as a function scope, it must
/// treat a `Func` frame the same way.
pub(crate) fn is_function_like(keyword: &str) -> bool {
    matches!(keyword, "Function" | "Sub" | "Func")
}

/// True when a `block_opener` match found INSIDE a
/// `Type`/`Enum`/`Interface`/`Union`/`Asm` body is not a genuine nested
/// declaration, but a member/variant/mnemonic row that merely collides
/// textually with a block keyword. See `open_declaration`'s per-arm doc
/// comments (`Type`, `Enum`/`Interface`) and the `Asm` case below for the
/// corpus evidence behind each branch.
///
/// `Enum`/`Interface`/`Union` bodies are pure flat lists (member
/// signatures or variants) with no inline-method carve-out, so ANY
/// block-opener match there is unconditionally a member row. `Asm` bodies
/// are assembly, never declarations — real corpus shape:
/// `tests/conformance/asm/test_asm_two_blocks.ml`'s `Sub Rbx, Rax` mnemonic
/// row matches `block_opener` as a `Sub` declaration (the operand row
/// starts with the exact block keyword `Sub` followed by a space); without
/// this guard that pushes a phantom frame, fabricates a `function` symbol
/// named from the operand, and desyncs the following `End Asm`/`End
/// Unsafe`. `Type` alone may contain real inline methods (MLH-2080), so it
/// defers to `member_shaped`.
pub(crate) fn skip_as_member_row(top_kw: Option<&str>, trimmed: &str, keyword: &str) -> bool {
    match top_kw {
        Some("Enum") | Some("Interface") | Some("Union") | Some("Asm") => true,
        Some("Type") => member_shaped(trimmed, keyword),
        _ => false,
    }
}

/// True when `trimmed` is a field/member row whose NAME happens to
/// collide with a block keyword — e.g. `Label As Str`, `Function As
/// SomeType` — signalled by the keyword being immediately followed by
/// ` As `. `keyword` must be the exact keyword `block_opener` matched on
/// this same line (this function replicates its Public/Private modifier
/// stripping so the two stay aligned).
///
/// False for a genuine declaration: `Function Cost(extra As Int) As Int`
/// has `Function` immediately followed by ` Cost(`, not ` As ` — the
/// return clause's ` As Int` is further down the line, past the
/// parameter list, so it never fools this check.
pub(crate) fn member_shaped(trimmed: &str, keyword: &str) -> bool {
    let mut rest = trimmed;
    for modifier in ["Public ", "Private "] {
        if let Some(r) = rest.strip_prefix(modifier) {
            rest = r.trim_start();
        }
    }
    rest.strip_prefix(keyword)
        .map(|after| after.starts_with(" As "))
        .unwrap_or(false)
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
    // How many UI elements of a given keyword have already opened under a
    // given parent scope (keyed by the parent's fqn, or "" for root-level
    // elements) — same-type siblings are the common case in the real
    // corpus (a single `Panel` commonly holds several `Label`s/`Switch`es;
    // `examples/ui/declarative_switch_png.ml` has 4 Labels and 3 Switches
    // under one Panel), and ancestry alone does not distinguish them.
    let mut ui_child_counts: HashMap<(String, &'static str), usize> = HashMap::new();
    // Defensive de-duplication for `contains_ui` edges, matching the
    // convention in `layout_extractor.rs` — each (parent, child) pair is
    // already unique once `ui_child_counts` disambiguates same-type
    // siblings' fqns, since this single-pass scanner visits each element
    // line exactly once, but this is cheap insurance against ever
    // double-emitting the same edge.
    let mut seen_contains_ui: HashSet<(String, String)> = HashSet::new();

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
        let inside_executable_scope = stack
            .last()
            .map(|b| is_function_like(b.keyword.as_str()))
            .unwrap_or(false);
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
        // that would otherwise be scanned as an opener. `closes_block`
        // covers both `End X` and the bare `Next` closer of a `For` frame.
        if let Some(closed) = closes_block(trimmed) {
            if let Some(open) = stack.pop() {
                if let Some(i) = open.symbol_idx {
                    symbols[i].end_line = line_no;
                    if is_function_like(&open.keyword) {
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

        // Rows directly inside a Type/Enum/Interface/Union/Asm body
        // (fields, variants, members, mnemonics) are a flat, non-nesting
        // list — NEVER nested block declarations — regardless of what
        // `block_opener` returns for them. This matters even when
        // `block_opener` DOES match a keyword: a field literally named
        // after a block keyword (real corpus shape: `Type Box { Label As
        // Str }` in 8 files under `tests/conformance/`, where `Label`
        // collides with the `Ui` DSL's `Label` element) would otherwise be
        // treated as a genuine opener — fabricating a `control`/etc.
        // symbol that does not exist, and pushing a phantom `OpenBlock`
        // with no matching `End Label` in the source, corrupting the stack
        // until the enclosing `End Type` line wrongly closes IT instead of
        // the real `Type` block. Every `Type`/`Enum`/`Interface`/`Union`
        // member row is already fully classified from `body` in
        // `open_declaration` when the ENCLOSING declaration line was
        // processed; an `Asm` row is already fully classified by
        // `ui::asm_symbol` when the `Asm` line itself was processed. There
        // is nothing more to do with any of them here.
        let in_declaration_body = matches!(
            stack.last().map(|b| b.keyword.as_str()),
            Some("Type") | Some("Enum") | Some("Interface") | Some("Union") | Some("Asm")
        );

        // Statement lines (everything that opens no block) contribute call
        // edges attributed to the innermost enclosing function. Rows
        // directly inside a Type/Enum/Interface body must be skipped here
        // too, exactly as they were before this scanner existed: without
        // this guard, a field row like `Mapper As Function(T) As R` has no
        // block opener either, so it would fall into this branch, find no
        // enclosing Function/Sub, and be misfiled as a top-level program
        // statement — fabricating a `<module>` entry symbol in files that
        // declare no such thing.
        if opener.is_none() {
            if in_declaration_body {
                // A field/variant/member row: not a statement, not a
                // declaration opener. Already accounted for by `body` in
                // `open_declaration`; nothing more to do with it here.
                continue;
            }

            // Interior rows of an open UI element (`Rect`/`Bg`/`Text`/…
            // attribute rows) or a `Define Style` bundle (`MinSize`/
            // `Margin`/… property rows) are already fully accounted for by
            // their opening line's own handling via `ui::ui_own_rows`. An
            // `Asm` block's own interior rows are handled by the
            // `in_declaration_body` guard above instead (not here): unlike
            // UI/Style rows, an Asm mnemonic row can itself match
            // `block_opener` (real corpus case: `Sub Rbx, Rax`), so it must
            // be excluded before the `opener.is_some()` branch below ever
            // sees it, not just here in the `opener.is_none()` branch.
            // Without this guard, every UI/Style row falls through to the
            // generic statement scanner below: it finds no enclosing
            // Function/Sub for most of them and misfiles them as top-level
            // program statements — fabricating a bloated synthetic
            // `<module>` entry symbol for every UI-DSL file.
            let already_consumed = match stack.last() {
                Some(b) => ui::UI_ELEMENTS.contains(&b.keyword.as_str()) || b.keyword == "Style",
                None => false,
            };
            if already_consumed {
                continue;
            }

            // This needs a mutable borrow of the stack to record locals and
            // catch regions, so it is a standalone block, resolved and
            // dropped BEFORE the immutable borrow below clones the
            // enclosing FQN — the two borrows cannot coexist.
            if let Some(owner) = stack
                .iter_mut()
                .rev()
                .find(|b| is_function_like(&b.keyword))
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
                .find(|b| is_function_like(&b.keyword))
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

        // `Enum`/`Interface` bodies are pure signature/variant lists — a
        // `Function`/`Sub` row here (real corpus case: `Interface
        // IInlineJob { Function Cost(extra As Int) As Int ... }`) is a
        // BARE signature with no body and no matching `End Function` of
        // its own. Treating it as a genuine opener would push a frame
        // nothing ever pops except the enclosing `End Interface`/`End
        // Enum` line, desyncing the stack — 212 real corpus (Function,
        // Interface) rows would do exactly this without an unconditional
        // skip here. `Union` bodies are the same shape (variants only, no
        // inline members). A `Type` body, unlike Enum/Interface/Union, may
        // declare real inline methods with full bodies (MLH-2080, real
        // corpus: `tests/conformance/interfaces/test_mlh2080_type_inline_methods.ml`'s
        // `Type BuildJob { ... Function Cost(extra As Int) As Int ... End
        // Function ... }`). Those MUST produce their own `function` symbol
        // and push their own frame so their `End Function`/`End Sub` pops
        // THEM, not the enclosing `Type` — unconditionally skipping here
        // (round 1's fix) truncated the Type's `end_line` and cascaded
        // into a stack desync on the following `End Type`. Skip only a row
        // shaped like a field whose name collides with a block keyword
        // (`Label As Str`, `Function As SomeType`), signalled by the
        // keyword being immediately followed by ` As ` — a real
        // declaration (`Function Cost(...)`, `Sub Push(x As T)`) never is.
        // An `Asm` body's mnemonic rows never nest, so they are always
        // skipped here too (see `skip_as_member_row`'s doc comment).
        if in_declaration_body {
            let top_kw = stack.last().map(|b| b.keyword.as_str());
            if skip_as_member_row(top_kw, trimmed, keyword) {
                continue;
            }
        }

        if keyword == "Unsafe" {
            if let Some(caps) = bodies::unsafe_capabilities(trimmed) {
                if let Some(owner) = stack.iter().rev().find(|b| is_function_like(&b.keyword)) {
                    edges.push(ExtractedEdge {
                        source_name: owner.fqn.clone(),
                        source_kind: "function".to_string(),
                        source_start_line: line_no,
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

        if ui::UI_ELEMENTS.contains(&keyword) {
            let stem = file_stem(rel_path);
            let parent = stack
                .iter()
                .rev()
                .find(|b| ui::UI_ELEMENTS.contains(&b.keyword.as_str()))
                .map(|b| b.fqn.clone());
            let base_fqn = match &parent {
                Some(p) => format!("{p}.{keyword}"),
                None => format!("{stem}.{keyword}"),
            };
            // Disambiguate same-type siblings under the same parent (the
            // common case in the real corpus — see `ui_child_counts`'s
            // doc comment above). The first occurrence keeps the plain
            // name; the 2nd, 3rd, … get a trailing ordinal.
            let scope_key = (parent.clone().unwrap_or_default(), keyword);
            let ordinal = ui_child_counts
                .entry(scope_key)
                .and_modify(|n| *n += 1)
                .or_insert(1);
            let fqn = if *ordinal == 1 {
                base_fqn
            } else {
                format!("{base_fqn}{ordinal}")
            };
            let mut sym = ui::ui_symbol(keyword, &fqn, line_no);
            if keyword == "Ui" {
                let mut m = sym.metadata.take().unwrap_or_default();
                for (k, v) in ui::ui_header_attrs(trimmed) {
                    m.insert(k, v);
                }
                sym.metadata = Some(m);
            }
            // Attribute rows belonging directly to this element (stops at
            // the first nested UI element, so a child's own Text/Rect
            // never bleeds into this element's metadata — see
            // `ui::ui_own_rows`'s doc comment).
            let own_rows = ui::ui_own_rows(source, idx, keyword);
            if let Some(m) = sym.metadata.as_mut() {
                for row in &own_rows {
                    if let Some((k, v)) = ui::ui_attribute(row) {
                        m.entry(k).or_insert(v);
                    }
                }
            }
            symbols.push(sym);
            let symbol_idx = Some(symbols.len() - 1);

            if let Some(p) = parent {
                if seen_contains_ui.insert((p.clone(), fqn.clone())) {
                    edges.push(ExtractedEdge {
                        source_name: p,
                        source_kind: "ui_container".to_string(),
                        // The line where this containment was observed —
                        // the child's own opening line — matching house
                        // convention (`layout_extractor.rs`'s `contains_ui`
                        // edges use the child's line, not a sentinel).
                        source_start_line: line_no,
                        source_language: "ml".to_string(),
                        target_name: fqn.clone(),
                        target_kind: Some("control".to_string()),
                        target_start_line: Some(line_no),
                        kind: "contains_ui".to_string(),
                        metadata: None,
                    });
                }
            }

            stack.push(OpenBlock {
                keyword: keyword.to_string(),
                fqn,
                start_line: line_no,
                symbol_idx,
                immutable_locals: Vec::new(),
                mutable_locals: Vec::new(),
                has_catch: false,
            });
            continue;
        }

        if keyword == "Asm" {
            let owner = stack
                .iter()
                .rev()
                .find(|b| is_function_like(&b.keyword))
                .map(|b| b.fqn.clone())
                .unwrap_or_else(|| file_stem(rel_path));
            let body = collect_block_body(source, idx, "Asm");
            symbols.push(ui::asm_symbol(&body, &owner, line_no));
            let symbol_idx = Some(symbols.len() - 1);
            stack.push(OpenBlock {
                keyword: keyword.to_string(),
                fqn: String::new(),
                start_line: line_no,
                symbol_idx,
                immutable_locals: Vec::new(),
                mutable_locals: Vec::new(),
                has_catch: false,
            });
            continue;
        }

        let parent_fqn = stack
            .iter()
            .rev()
            .find(|b| !b.fqn.is_empty())
            .map(|b| b.fqn.as_str())
            .unwrap_or("");

        // Type/Enum/Interface/Union bodies are classified from their rows,
        // so collect the block's lines up to its matching `End`. A `Type`
        // body needs its OWN collector: unlike Enum/Interface/Union, it may
        // contain real inline methods (MLH-2080) whose declaration line
        // and full statement body must be excluded from the member rows
        // handed to the field/variant classifier — see
        // `collect_type_member_rows`'s doc comment. `Union` bodies are
        // variants-only, the same flat shape as Enum/Interface, so they
        // share the generic collector.
        let body: Vec<&str> = match keyword {
            "Type" => collect_type_member_rows(source, idx),
            "Enum" | "Interface" | "Union" => collect_block_body(source, idx, keyword),
            _ => Vec::new(),
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
        let module_fqn = format!("{}.<module>", file_stem(rel_path));
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

/// File stem of a project-relative path: `src/Lib/Badge.ml` → `Badge`.
pub(crate) fn file_stem(rel_path: &str) -> String {
    rel_path
        .rsplit(['/', '\\'])
        .next()
        .and_then(|f| f.split('.').next())
        .unwrap_or("module")
        .to_string()
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

/// Lines strictly inside a `Type` body that are the type's OWN member
/// rows (fields/variants) — never a nested inline method's declaration
/// line or any line from its body.
///
/// MiniLang `Type` bodies may declare methods with full bodies (MLH-2080,
/// real corpus: `Type BuildJob` in
/// `tests/conformance/interfaces/test_mlh2080_type_inline_methods.ml`
/// declares three `Function`s). A generic whole-body scan
/// (`collect_block_body`) would hand the field/variant classifier in
/// `decls::open_declaration`'s `Type` arm every line of a method's
/// signature and body too — misreading `Function Cost(extra As Int) As
/// Int` as a variant row (its `lhs` contains `(`) and `Return weight * 2
/// + extra` as another, polluting the type's `variants` metadata with
/// garbage like `Function/1`, `Return/0`.
///
/// A method's declaration line and everything up to its own matching
/// `End Function`/`End Sub` is skipped entirely and excluded from the
/// output. A field/variant row whose NAME happens to collide with
/// `Function`/`Sub` (immediately followed by ` As `, e.g. `Function As
/// SomeType` — see `member_shaped`) is still a genuine member row and is
/// kept; only a REAL method declaration is skipped.
pub(crate) fn collect_type_member_rows(source: &str, open_idx: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 1usize;
    let mut lines = source.lines().skip(open_idx + 1);
    while let Some(line) = lines.next() {
        let trimmed = strip_comment(line).trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(closed) = block_closer(trimmed) {
            if closed == "Type" {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            continue;
        }
        if block_opener(trimmed) == Some("Type") {
            depth += 1;
        }
        if let Some(kw) = block_opener(trimmed)
            && is_function_like(kw)
            && !member_shaped(trimmed, kw)
        {
            // A real inline method: fast-forward past its own body to its
            // matching `End Function`/`End Sub`, contributing NONE of
            // those lines to the member-row output.
            let mut method_depth = 1usize;
            while method_depth > 0 {
                let Some(inner) = lines.next() else { break };
                let inner_trimmed = strip_comment(inner).trim();
                if inner_trimmed.is_empty() {
                    continue;
                }
                if let Some(inner_closed) = block_closer(inner_trimmed) {
                    if inner_closed == kw {
                        method_depth -= 1;
                    }
                    continue;
                }
                if block_opener(inner_trimmed) == Some(kw) {
                    method_depth += 1;
                }
            }
            continue;
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
