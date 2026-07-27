//! MiniLang declarative `Ui` DSL and inline `Asm` blocks.

use crate::parsing::ExtractedSymbol;

use super::decls::meta;
use super::{block_closer, block_opener, strip_comment};

/// UI elements that open a nesting level and become graph nodes. `Ui` is
/// the root container; the rest are controls.
///
/// Verified against the real corpus (`examples/ui/*.ml`,
/// `tests/conformance/ui/*.ml` — 58 files opening with `^Ui `): every
/// entry here is backed by real `End <Element>` usage.
///
/// `Switch` is included as a UI toggle control (7 real `End Switch`
/// occurrences across 3 files: `declarative_switch_png.ml`,
/// `declarative_gallery_png.ml`, `test_ui_block_switch_headless.ml`), NOT
/// as control flow. MiniLang has no control-flow `Switch`/`Case`/`End
/// Switch` construct at all — `Match`/`Case` and `SelectChannel`/`Case`
/// cover that role instead. This was verified against the compiler's own
/// parser (`Frontend/Parser.Statements.vb`, which defines `Case` clauses
/// only under `Match`/`SelectChannel`) and `LANGUAGE.md`, which never
/// documents a control-flow `Switch` either. Every real `Switch` in the
/// corpus is this toggle element.
pub(crate) const UI_ELEMENTS: &[&str] = &[
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

/// Attribute rows inside a UI element — they configure the enclosing
/// element rather than opening a nesting level.
pub(crate) fn ui_attribute(trimmed: &str) -> Option<(String, String)> {
    for key in [
        "Rect", "Bg", "Text", "Style", "Border", "Gradient", "Shadow",
    ] {
        if let Some(rest) = trimmed.strip_prefix(key) {
            if rest.is_empty() || rest.starts_with(' ') {
                let value = rest.trim();
                // `Text "…"` carries a quoted string; the rest are bare.
                let value = value
                    .strip_prefix('"')
                    .and_then(|v| v.find('"').map(|e| &v[..e]))
                    .unwrap_or(value);
                return Some((key.to_ascii_lowercase(), value.to_string()));
            }
        }
    }
    None
}

/// Build a UI element symbol. `fqn` is the dotted path from the file stem
/// through the element's ancestors; the caller (`mod.rs`'s UI-element
/// branch) appends a per-parent ordinal suffix for the second and later
/// same-type sibling, since ancestry alone does not distinguish e.g. the
/// 2nd and 3rd `Label` under one `Panel` (a common real-corpus shape —
/// `examples/ui/declarative_switch_png.ml`'s single `Panel` holds 4
/// `Label`s and 3 `Switch`es).
pub(crate) fn ui_symbol(element: &str, fqn: &str, line_no: u32) -> ExtractedSymbol {
    let kind = if element == "Ui" {
        "ui_container"
    } else {
        "control"
    };
    ExtractedSymbol {
        name: fqn.to_string(),
        kind: kind.to_string(),
        start_line: line_no,
        end_line: 0,
        metadata: meta(&[("element", element.to_string())]),
    }
}

/// Header attributes on the `Ui` line itself: `Ui Width 420 Height 160 Bg
/// bg`.
///
/// `Window` is a bare flag (no value) that may immediately follow `Ui`
/// with no paired value of its own — `Ui Window Width 360 Height 220 Bg
/// bg` (13 of the 58 real corpus files with a `Ui` header — corrected
/// from an earlier "284" count, which counted the corpus duplicated
/// across `.claude/worktrees` copies instead of excluding them;
/// `examples/ui/declarative_window_png.ml` and friends). It must be
/// skipped before pairing starts, or every subsequent key/value slot
/// shifts by one and `width`/`height`/`bg` all come out wrong
/// (`("window", "Width")`, `("360", "Height")`, …). Windowed vs headless
/// is real semantic content, so it is recorded as `window: true` rather
/// than silently discarded.
pub(crate) fn ui_header_attrs(trimmed: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    let mut i = 1usize; // skip the `Ui` keyword
    if tokens.get(i) == Some(&"Window") {
        out.push(("window".to_string(), "true".to_string()));
        i += 1;
    }
    while i + 1 < tokens.len() {
        out.push((tokens[i].to_ascii_lowercase(), tokens[i + 1].to_string()));
        i += 2;
    }
    out
}

/// Attribute rows that belong directly to the UI element opened at
/// `open_idx` — everything up to the element's own `End <keyword>` or the
/// first line that opens ANY nested block, whichever comes first.
///
/// A generic whole-body scan (`collect_block_body`) walks the WHOLE
/// element body, including any nested child's rows — it only tracks
/// depth for the SAME keyword, not other block keywords — so a child's
/// `Text`/`Rect` rows would otherwise fold into the ancestor's metadata
/// too (e.g. `Panel > Label` with a `Text "…"` row would leak `text` onto
/// the Panel).
///
/// The break condition covers ANY `block_opener` match, not just UI
/// elements: a nested `Define Style <name> … End Style` bundle (see
/// `mod.rs::block_opener`'s `Define Style` handling) is not a UI element,
/// but its own interior rows (`Bg accent`, `Border accent 2`, …) are just
/// as much "not this element's own attributes" as a nested `Label`'s are
/// — the real corpus (`tests/conformance/ui/test_ui_class_bg_headless.ml`
/// and 7 other files) has bundles whose properties would otherwise
/// silently attach to the enclosing `Ui`/`Panel` as if they were its own
/// (a `Border` row is especially misleading this way: no `Ui` header ever
/// sets `border`, so a bundle's `Border` row would land on the root
/// container looking exactly like the window's own border).
///
/// This assumes an element's own attribute rows always precede its first
/// child/nested block — true of every real file inspected across the
/// corpus (`examples/ui/*.ml`, `tests/conformance/ui/*.ml`), but NOT
/// verified as a hard grammar guarantee. If a future file ever placed an
/// attribute row AFTER a child, that row would be silently dropped here
/// rather than folded — a false negative, not a crash or a bleed.
pub(crate) fn ui_own_rows<'a>(source: &'a str, open_idx: usize, keyword: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    for line in source.lines().skip(open_idx + 1) {
        let trimmed = strip_comment(line).trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(closed) = block_closer(trimmed) {
            if closed == keyword {
                break;
            }
            // A different `End X` here belongs to a nested block that
            // already triggered the break below when it opened.
            continue;
        }
        if block_opener(trimmed).is_some() {
            // Any nested block — a UI element, a `Define Style` bundle,
            // or (defensively) anything else the scanner recognizes as an
            // opener — starts a scope that is not this element's own
            // attributes.
            break;
        }
        out.push(trimmed);
    }
    out
}

/// Parse an inline `Asm` block body into mnemonics and In/Out bindings.
///
/// Real grammar (verified against the compiler's own recursive-descent
/// parser, `Frontend/Parser.Statements.vb::ParseAsmLine`): `In`/`Out` rows
/// are BARE, comma-separated variable names with NO `As Type` clause —
/// `RequireEol()` is called immediately after the name list, so a type
/// clause is not even syntactically legal there. Every asm in/out slot is
/// hardcoded to `Int` by the compiler itself (`InlineAsmStmt.ToIR` in
/// `Core/AST/StatementNodes.Unsafe.vb` declares each `__asm_inN`/
/// `__asm_outN` temporary as `VarType.Int` unconditionally; `AsmLine`/
/// `InlineAsmStmt` carry no type field at all). `LANGUAGE.md`'s `In x As
/// Int` example matches neither the parser nor any of the 47 real
/// `Asm`-bearing corpus files (all shaped like `In x, y` / `Out result`,
/// e.g. `tests/conformance/asm/test_asm_arithmetic.ml`).
pub(crate) fn asm_symbol(body: &[&str], owner: &str, line_no: u32) -> ExtractedSymbol {
    let mut mnemonics: Vec<String> = Vec::new();
    let mut inputs: Vec<String> = Vec::new();
    let mut outputs: Vec<String> = Vec::new();

    for row in body {
        if let Some(rest) = row.strip_prefix("In ") {
            inputs.extend(
                rest.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from),
            );
            continue;
        }
        if let Some(rest) = row.strip_prefix("Out ") {
            outputs.extend(
                rest.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from),
            );
            continue;
        }
        if let Some(word) = row.split_whitespace().next() {
            mnemonics.push(word.to_string());
        }
    }

    ExtractedSymbol {
        name: format!("{owner}.<asm>"),
        kind: "inline_asm".to_string(),
        start_line: line_no,
        // Backfilled from the real `End Asm` line by the scanner's
        // generic closer handling (via `symbol_idx`), the same way every
        // other block-producing symbol in this extractor gets its
        // `end_line` — NOT hand-counted from `body.len()`, which would
        // undercount whenever the block contains blank or comment-only
        // lines (both occur in real Asm blocks, e.g.
        // `tests/conformance/asm/test_asm_clobber_all_gpr.ml`).
        end_line: 0,
        metadata: meta(&[
            ("owner", owner.to_string()),
            ("mnemonics", mnemonics.join("||")),
            ("inputs", inputs.join("||")),
            ("outputs", outputs.join("||")),
        ]),
    }
}
