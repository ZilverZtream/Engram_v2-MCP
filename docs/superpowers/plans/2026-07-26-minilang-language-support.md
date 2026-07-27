# MiniLang (`.ml`) Language Support Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Engram index, graph, and reason about MiniLang `.ml`/`.mlinc` source at parity with its existing VB.NET support.

**Architecture:** MiniLang has no tree-sitter grammar, so a hand-rolled line-based extractor (`engram_index/src/ml_extractor/`) parses its `End <keyword>` block structure — the same approach `asp_classic_extractor.rs` uses. Work proceeds outward in dependency order: ingestion plumbing first (which alone makes `.ml` searchable), then the extractor built construct-by-construct, then the parity sites across `engram_server` that currently dispatch on VB.

**Tech Stack:** Rust (edition 2024), `regex`, `tree_sitter` (not used for MiniLang), redb graph store, tantivy index.

## Global Constraints

- Spec: `docs/superpowers/specs/2026-07-26-minilang-language-support-design.md`. The **corpus** (`C:\Users\Dennis\source\repos\MiniLangCompiler\src`, `examples`, `benchmarks`) is the grammar authority, **not** `LANGUAGE.md`.
- `engram_index/src/lib.rs` declares `#![deny(clippy::unwrap_used)]` and `#![deny(clippy::print_stdout)]`. Use `.expect("…")` in `LazyLock` regex construction (existing idiom); never `unwrap()` in non-test code.
- Rust edition 2024: `gen` is a reserved keyword — use `gen_`.
- Build/verify with the Bash tool, **not** PowerShell with `2>&1` — benign stderr WARNs become `NativeCommandError` and report exit 255 on a green suite.
- `cargo test --all` may OOM. Use `cargo test -p engram_index -p engram_server --tests --lib --no-fail-fast`.
- CI-equivalent local check: `cargo fmt --all && cargo check --all-targets`.
- Symbol `kind` strings pass through to graph node types verbatim via the generic branch in `ingest_service.rs`; no node-type registry edit is needed.
- Edge target paths must be **project-relative**. Absolute targets are rejected by the ingest safety check.
- No customer strings in source or fixtures. MiniLang fixtures use synthetic names.

## File Structure

**Created:**
- `crates/engram_index/src/ml_extractor/mod.rs` — `extract_ml()` entry, comment stripper, block scanner, namespace/FQN stack, dispatch
- `crates/engram_index/src/ml_extractor/decls.rs` — Namespace, Function/Sub, Type, Enum, Interface, Const, Include, Declare, Extern parsing
- `crates/engram_index/src/ml_extractor/bodies.rs` — call sites, capabilities, concurrency, SIMD, ownership metadata
- `crates/engram_index/src/ml_extractor/ui.rs` — the `Ui` DSL and inline `Asm`
- `crates/engram_index/src/language_diagnostics/minilang.rs` — pre-edit risk heuristics
- `crates/engram_index/tests/ml_extractor_test.rs` — construct-by-construct unit tests
- `crates/engram_index/tests/ml_corpus_smoke_test.rs` — regression floor over representative shapes

**Modified:**
- `crates/engram_core/src/types.rs` — `guess_language`
- `crates/engram_index/src/lib.rs` — module declaration
- `crates/engram_index/src/parsing.rs` — `ext_to_static`
- `crates/engram_index/src/hybrid.rs` — extractor dispatch arm
- `crates/engram_index/src/language_diagnostics/mod.rs` — `LanguageFamily::MiniLang`
- `crates/engram_graph/src/store.rs` — `EdgeKind::TestOracle`
- `crates/engram_server/src/models/requests.rs` — `ProjectType::MiniLang`
- `crates/engram_server/src/utils/files.rs` — extension presets
- `crates/engram_server/src/services/ingest_service.rs` — raw-kind map
- `crates/engram_server/src/services/business_logic_service.rs` — `detect_language`, method-name regex
- `crates/engram_server/src/services/full_project_migration_service.rs` — `extract_ml_method_body`
- `crates/engram_server/src/services/pre_commit_review_service/gates.rs` — complexity + style gating
- `crates/engram_server/src/handlers/planning_tools.rs` — interface pairing, api-path
- `crates/engram_server/src/services/produce_claude_md_service.rs` — globs, display name
- `crates/engram_server/src/services/code_review_ingest_service.rs` — language tag
- `crates/engram_server/src/handlers/access_layer_tools.rs` — diagnostics family mapping

---

### Task 1: Ingestion plumbing — make `.ml` visible

Nothing downstream can be tested until the walker yields `.ml` paths. On its own this task makes MiniLang files searchable (chunked and indexed as text), which is independently valuable and verifiable.

**Files:**
- Modify: `crates/engram_core/src/types.rs` (`guess_language`, ~line 29)
- Modify: `crates/engram_index/src/parsing.rs` (`ext_to_static`, ~line 525)
- Modify: `crates/engram_server/src/models/requests.rs` (`ProjectType`, ~line 128; `from_registry_str`, ~line 192)
- Modify: `crates/engram_server/src/utils/files.rs` (`default_exts`, `exts_for_project_type_enum`)
- Test: `crates/engram_core/src/types.rs` (inline `#[cfg(test)]`), `crates/engram_server/src/utils/files.rs` (inline `#[cfg(test)]`)

**Interfaces:**
- Consumes: nothing.
- Produces: `ProjectType::MiniLang`; `guess_language()` returns `"minilang"` for `ml`/`mlinc`; `minilang_exts() -> Vec<&'static str>`.

- [ ] **Step 1: Write the failing test for `guess_language`**

Append to the `#[cfg(test)] mod tests` block in `crates/engram_core/src/types.rs` (create the block at end of file if absent):

```rust
#[test]
fn minilang_extensions_map_to_minilang() {
    use std::path::Path;
    assert_eq!(guess_language(Path::new("Std.Collections.List.ml")), "minilang");
    assert_eq!(guess_language(Path::new("shared.mlinc")), "minilang");
    // Case-insensitive: guess_language lowercases the extension.
    assert_eq!(guess_language(Path::new("Kernel.ML")), "minilang");
    // Unrelated extensions are untouched.
    assert_eq!(guess_language(Path::new("Form1.vb")), "vbnet");
}
```

- [ ] **Step 2: Run it to make sure it fails**

Run: `cargo test -p engram_core minilang_extensions_map_to_minilang`
Expected: FAIL — `assertion \`left == right\` failed: left: "text", right: "minilang"`

- [ ] **Step 3: Implement `guess_language`**

In `crates/engram_core/src/types.rs`, add to the match in `guess_language`, immediately after the `"vb" => "vbnet",` arm:

```rust
        // MiniLang — native systems language, VB-flavoured block syntax.
        // `.ml` is claimed unconditionally; an OCaml repo's files stay
        // searchable but yield no MiniLang symbols. Decision recorded in
        // docs/superpowers/specs/2026-07-26-minilang-language-support-design.md.
        "ml" | "mlinc" => "minilang",
```

- [ ] **Step 4: Run it to make sure it passes**

Run: `cargo test -p engram_core minilang_extensions_map_to_minilang`
Expected: PASS

- [ ] **Step 5: Write the failing test for the project type and extension preset**

Append to the `#[cfg(test)] mod tests` block in `crates/engram_server/src/utils/files.rs` (create it at end of file if absent):

```rust
#[test]
fn minilang_project_type_round_trips_and_indexes_ml() {
    use crate::models::ProjectType;

    // Registry strings and aliases all resolve to the MiniLang variant.
    for raw in ["minilang", "MiniLang", "mini_lang", "ml"] {
        assert_eq!(
            ProjectType::from_registry_str(raw),
            Some(ProjectType::MiniLang),
            "registry string {raw:?} should resolve to MiniLang"
        );
    }
    assert_eq!(ProjectType::MiniLang.as_str(), "minilang");

    // The preset indexes MiniLang source, its goldens, and the polyglot
    // compiler sources that live alongside it.
    let exts = exts_for_project_type_enum(ProjectType::MiniLang);
    for required in ["ml", "mlinc", "expected", "error", "exitcode", "vb", "cs", "rs"] {
        assert!(exts.contains(&required), "MiniLang preset must index {required:?}");
    }

    // Goldens are MiniLang-only: their names are too generic for other repos.
    let general = exts_for_project_type_enum(ProjectType::General);
    assert!(general.contains(&"ml"), "general preset must index .ml");
    assert!(general.contains(&"mlinc"), "general preset must index .mlinc");
    assert!(!general.contains(&"expected"), "general preset must NOT index .expected");
}
```

- [ ] **Step 6: Run it to make sure it fails**

Run: `cargo test -p engram_server minilang_project_type_round_trips_and_indexes_ml`
Expected: FAIL to compile — `no variant named \`MiniLang\` found for enum \`ProjectType\``

- [ ] **Step 7: Add the `ProjectType::MiniLang` variant**

In `crates/engram_server/src/models/requests.rs`, add to the `ProjectType` enum after the `C` variant:

```rust
    /// MiniLang — native systems language compiled by MiniLangCompiler.
    /// Indexes `.ml`/`.mlinc` alongside the polyglot compiler sources and
    /// conformance-test goldens that share the repository.
    #[serde(alias = "mini_lang", alias = "ml")]
    MiniLang,
```

Add to `as_str`:

```rust
            Self::MiniLang => "minilang",
```

Add to `from_registry_str`, as a new `else if` branch before the final fallback:

```rust
        } else if ["minilang", "mini_lang", "ml"]
            .iter()
            .any(|x| v.eq_ignore_ascii_case(x))
        {
            Some(Self::MiniLang)
```

- [ ] **Step 8: Add the extension preset**

In `crates/engram_server/src/utils/files.rs`, add after `c_exts()`:

```rust
/// MiniLang projects. A MiniLang compiler repository is polyglot: MiniLang
/// stdlib and tests (`ml`/`mlinc`), the compiler itself (VB.NET or C#), and
/// C/Rust/Go external-ABI fixtures. Conformance tests pair each source file
/// with `expected`/`error`/`exitcode` goldens, which the extractor links via
/// `test_oracle` edges.
fn minilang_exts() -> Vec<&'static str> {
    vec![
        "ml", "mlinc", "expected", "error", "exitcode", "vb", "vbproj", "sln", "cs", "csproj",
        "c", "rs", "go", "ps1", "sh", "md", "json", "yaml", "yml", "txt", "snapshot",
    ]
}
```

Add the dispatch arm in `exts_for_project_type_enum`:

```rust
        ProjectType::MiniLang => minilang_exts(),
```

Add `"ml"` and `"mlinc"` to the `default_exts()` vector, after `"vb",`:

```rust
        "ml", "mlinc",
```

- [ ] **Step 9: Add `ext_to_static` support**

In `crates/engram_index/src/parsing.rs`, add to the `ext_to_static` match after the `"vb" => "vb",` arm:

```rust
        "ml" => "ml",
        "mlinc" => "mlinc",
```

- [ ] **Step 10: Run the tests to verify they pass**

Run: `cargo test -p engram_core -p engram_server minilang`
Expected: PASS (2 tests)

- [ ] **Step 11: Verify the workspace still compiles**

Run: `cargo fmt --all && cargo check --all-targets`
Expected: clean. The `exts_for_project_type_enum` match is exhaustive over `ProjectType`, so a missing arm would have failed here.

- [ ] **Step 12: Commit**

```bash
git add crates/engram_core/src/types.rs crates/engram_index/src/parsing.rs crates/engram_server/src/models/requests.rs crates/engram_server/src/utils/files.rs
git commit -m "feat(minilang): index .ml/.mlinc files

Adds ProjectType::MiniLang with a polyglot extension preset, maps
.ml/.mlinc to the minilang language, and puts them in the general
preset. MiniLang source is now walked, chunked, and searchable --
previously the walker never yielded these paths at all."
```

---

### Task 2: Extractor skeleton — comment stripping, block scanner, namespaces

Establishes the module and the two primitives every later task builds on: a comment stripper that respects string literals, and a block scanner that tracks `End <keyword>` nesting. Delivers namespace symbols.

**Files:**
- Create: `crates/engram_index/src/ml_extractor/mod.rs`
- Modify: `crates/engram_index/src/lib.rs`
- Test: `crates/engram_index/tests/ml_extractor_test.rs`

**Interfaces:**
- Consumes: `ExtractedSymbol`, `ExtractedEdge` from `crate::parsing`.
- Produces:
  - `pub fn extract_ml(abs_path: &Path, rel_path: &str, source: &str) -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>)`
  - `pub(crate) fn strip_comment(line: &str) -> &str`
  - `pub(crate) fn block_opener(trimmed: &str) -> Option<&'static str>` — returns the block keyword a line opens
  - `pub(crate) fn block_closer(trimmed: &str) -> Option<String>` — returns the keyword an `End X` line closes

- [ ] **Step 1: Write the failing tests**

Create `crates/engram_index/tests/ml_extractor_test.rs`:

```rust
use engram_index::ml_extractor::extract_ml;
use std::path::Path;

/// Helper: run the extractor on a source string with a stable fake path.
fn run(src: &str) -> (Vec<engram_index::parsing::ExtractedSymbol>, Vec<engram_index::parsing::ExtractedEdge>) {
    extract_ml(Path::new("C:/proj/Sample.ml"), "Sample.ml", src)
}

#[test]
fn strips_comments_but_not_string_contents() {
    // All three comment markers are stripped; markers inside string
    // literals are preserved. MiniLang has no character-literal syntax,
    // so a bare ' is always a comment.
    let src = "\
Namespace Demo
    Function Marker() As Str
        Return \"a ' b # c // d\"
    End Function
End Namespace
' trailing comment
# hash comment
// slash comment
";
    let (syms, _) = run(src);
    let f = syms.iter().find(|s| s.kind == "function").expect("function symbol");
    assert_eq!(f.name, "Demo.Marker");
}

#[test]
fn nested_namespaces_build_fqn() {
    let src = "\
Namespace Std
    Namespace Collections
        Function ListCount() As Int
            Return 0
        End Function
    End Namespace
End Namespace
";
    let (syms, _) = run(src);
    let ns: Vec<&str> = syms
        .iter()
        .filter(|s| s.kind == "namespace")
        .map(|s| s.name.as_str())
        .collect();
    assert!(ns.contains(&"Std"), "got namespaces {ns:?}");
    assert!(ns.contains(&"Std.Collections"), "got namespaces {ns:?}");

    let f = syms.iter().find(|s| s.kind == "function").expect("function symbol");
    assert_eq!(f.name, "Std.Collections.ListCount");
}

#[test]
fn control_flow_blocks_do_not_corrupt_nesting() {
    // End If / End While / End Try must be balanced by the scanner but
    // must not close the enclosing function.
    let src = "\
Namespace Demo
    Function Complex(n As Int) As Int
        If n > 0
            While n > 0
                Set n To n - 1
            End While
        End If
        Try
            Return n
        Finally
            Say n
        End Try
    End Function
    Function After() As Int
        Return 1
    End Function
End Namespace
";
    let (syms, _) = run(src);
    let fns: Vec<&str> = syms
        .iter()
        .filter(|s| s.kind == "function")
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(fns, vec!["Demo.Complex", "Demo.After"]);

    let complex = syms.iter().find(|s| s.name == "Demo.Complex").expect("Complex");
    assert_eq!(complex.start_line, 2);
    assert_eq!(complex.end_line, 13);
}
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p engram_index --test ml_extractor_test`
Expected: FAIL to compile — `unresolved import \`engram_index::ml_extractor\``

- [ ] **Step 3: Create the module**

Create `crates/engram_index/src/ml_extractor/mod.rs`:

```rust
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
    /// 1-based line where the block opened.
    pub start_line: u32,
    /// Index into the symbol vector for the symbol this block produced,
    /// so the scanner can backfill `end_line` when the block closes.
    pub symbol_idx: Option<usize>,
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

    for (idx, raw_line) in source.lines().enumerate() {
        let line_no = (idx + 1) as u32;
        let trimmed = strip_comment(raw_line).trim();
        if trimmed.is_empty() {
            continue;
        }

        // Closing lines first: `End Function` also starts with a keyword
        // that would otherwise be scanned as an opener.
        if let Some(closed) = block_closer(trimmed) {
            if let Some(open) = stack.pop() {
                if let Some(i) = open.symbol_idx {
                    symbols[i].end_line = line_no;
                }
                debug_assert_eq!(
                    open.keyword, closed,
                    "MiniLang block mismatch at {rel_path}:{line_no}"
                );
            }
            continue;
        }

        let Some(keyword) = block_opener(trimmed) else {
            continue;
        };

        let parent_fqn = stack
            .iter()
            .rev()
            .find(|b| !b.fqn.is_empty())
            .map(|b| b.fqn.as_str())
            .unwrap_or("");

        let (fqn, symbol_idx) = decls::open_declaration(
            keyword,
            trimmed,
            parent_fqn,
            line_no,
            &mut symbols,
            &mut edges,
        );

        stack.push(OpenBlock {
            keyword: keyword.to_string(),
            fqn,
            start_line: line_no,
            symbol_idx,
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

    (symbols, edges)
}
```

- [ ] **Step 4: Create a minimal `decls.rs` so the module compiles**

Create `crates/engram_index/src/ml_extractor/decls.rs`:

```rust
//! MiniLang declaration parsing: Namespace, Function/Sub, Type, Enum,
//! Interface, Const, Include, Declare, Extern.

use crate::parsing::{ExtractedEdge, ExtractedSymbol};
use std::collections::HashMap;

/// Parse the declaration a block-opening line introduces.
///
/// Returns `(fqn, symbol_index)`. The FQN is empty for control-flow blocks,
/// which contribute no naming scope. The symbol index lets the scanner
/// backfill `end_line` when the block closes.
pub(crate) fn open_declaration(
    keyword: &str,
    trimmed: &str,
    parent_fqn: &str,
    line_no: u32,
    symbols: &mut Vec<ExtractedSymbol>,
    edges: &mut Vec<ExtractedEdge>,
) -> (String, Option<usize>) {
    let _ = edges;
    match keyword {
        "Namespace" => {
            let Some(name) = trimmed.strip_prefix("Namespace").map(str::trim) else {
                return (String::new(), None);
            };
            if name.is_empty() {
                return (String::new(), None);
            }
            let fqn = if parent_fqn.is_empty() {
                name.to_string()
            } else {
                format!("{parent_fqn}.{name}")
            };
            symbols.push(ExtractedSymbol {
                name: fqn.clone(),
                kind: "namespace".to_string(),
                start_line: line_no,
                end_line: 0,
                metadata: None,
            });
            (fqn, Some(symbols.len() - 1))
        }
        _ => (String::new(), None),
    }
}

/// Build a metadata map, skipping empty values so the graph does not carry
/// noise keys.
pub(crate) fn meta(pairs: &[(&str, String)]) -> Option<HashMap<String, String>> {
    let map: HashMap<String, String> = pairs
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| ((*k).to_string(), v.clone()))
        .collect();
    if map.is_empty() { None } else { Some(map) }
}
```

- [ ] **Step 5: Create empty `bodies.rs` and `ui.rs` stubs**

Create `crates/engram_index/src/ml_extractor/bodies.rs`:

```rust
//! MiniLang call sites, capabilities, concurrency, SIMD, and ownership
//! metadata. Populated in Task 5.
```

Create `crates/engram_index/src/ml_extractor/ui.rs`:

```rust
//! MiniLang declarative `Ui` DSL and inline `Asm`. Populated in Task 6.
```

- [ ] **Step 6: Declare the module**

In `crates/engram_index/src/lib.rs`, add after `pub mod layout_extractor;`:

```rust
pub mod ml_extractor;
```

- [ ] **Step 7: Implement Function/Sub declarations so the tests pass**

In `crates/engram_index/src/ml_extractor/decls.rs`, add the `Function`/`Sub` arm to `open_declaration`'s match, before the `_` arm:

```rust
        "Function" | "Sub" => {
            let Some(name) = declaration_name(trimmed, keyword) else {
                return (String::new(), None);
            };
            let fqn = if parent_fqn.is_empty() {
                name.clone()
            } else {
                format!("{parent_fqn}.{name}")
            };
            symbols.push(ExtractedSymbol {
                name: fqn.clone(),
                kind: "function".to_string(),
                start_line: line_no,
                end_line: 0,
                metadata: meta(&[("is_sub", (keyword == "Sub").to_string())]),
            });
            (fqn, Some(symbols.len() - 1))
        }
```

Add the name parser to the same file:

```rust
/// The declared name on a `Function`/`Sub`/`Type`/`Enum`/`Interface` line.
///
/// The name is captured independently of what follows it, because MiniLang
/// puts generic parameters BETWEEN the name and the parameter list:
/// `Function BTreeMap_Get Of K, V(Borrow tree As …)`. A pattern demanding
/// `name(` would miss every generic declaration in the standard library.
pub(crate) fn declaration_name(trimmed: &str, keyword: &str) -> Option<String> {
    let mut rest = trimmed;
    for modifier in ["Public ", "Private "] {
        if let Some(r) = rest.strip_prefix(modifier) {
            rest = r.trim_start();
        }
    }
    let rest = rest.strip_prefix(keyword)?.trim_start();
    let name: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() { None } else { Some(name) }
}
```

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cargo test -p engram_index --test ml_extractor_test`
Expected: PASS (3 tests)

- [ ] **Step 9: Commit**

```bash
git add crates/engram_index/src/ml_extractor/ crates/engram_index/src/lib.rs crates/engram_index/tests/ml_extractor_test.rs
git commit -m "feat(minilang): extractor skeleton with block scanner

Comment stripper respects string literals and their escapes; MiniLang
has no char-literal syntax so ' is unambiguous. Block scanner tracks
End <keyword> nesting including control-flow blocks, which emit no
symbols but must balance or they close the enclosing function early.
Namespaces and Function/Sub declarations produce FQN-scoped symbols."
```

---

### Task 3: Types — struct, union, enum, interface, and `Implements`

**Files:**
- Modify: `crates/engram_index/src/ml_extractor/decls.rs`
- Test: `crates/engram_index/tests/ml_extractor_test.rs`

**Interfaces:**
- Consumes: `open_declaration`, `declaration_name`, `meta` from Task 2.
- Produces: symbol kinds `struct`, `union`, `enum`, `interface`; `implements_interface` edges.

- [ ] **Step 1: Write the failing tests**

Append to `crates/engram_index/tests/ml_extractor_test.rs`:

```rust
#[test]
fn type_with_field_rows_is_a_struct() {
    let src = "\
Namespace Demo
    Type Point
        X As Int
        Y As Int
    End Type
End Namespace
";
    let (syms, _) = run(src);
    let t = syms.iter().find(|s| s.kind == "struct").expect("struct symbol");
    assert_eq!(t.name, "Demo.Point");
    let m = t.metadata.as_ref().expect("metadata");
    assert_eq!(m.get("fields").map(String::as_str), Some("X:Int||Y:Int"));
}

#[test]
fn type_with_variant_rows_is_a_union() {
    let src = "\
Type Shape
    Circle(radius As Int)
    Rectangle(w As Int, h As Int)
    Point
End Type
";
    let (syms, _) = run(src);
    let t = syms.iter().find(|s| s.kind == "union").expect("union symbol");
    assert_eq!(t.name, "Shape");
    let m = t.metadata.as_ref().expect("metadata");
    assert_eq!(m.get("variants").map(String::as_str), Some("Circle/1||Rectangle/2||Point/0"));
}

#[test]
fn generic_type_records_parameters_and_constraints() {
    let src = "\
Namespace Std
    Public Type BTreeMap Of K As Ordered, V As Droppable
        Handle As Int
    End Type
End Namespace
";
    let (syms, _) = run(src);
    let t = syms.iter().find(|s| s.kind == "struct").expect("struct symbol");
    assert_eq!(t.name, "Std.BTreeMap");
    let m = t.metadata.as_ref().expect("metadata");
    assert_eq!(m.get("generic_params").map(String::as_str), Some("K:Ordered||V:Droppable"));
    assert_eq!(m.get("access").map(String::as_str), Some("Public"));
}

#[test]
fn implements_clause_emits_an_edge() {
    let src = "\
Namespace Std
    Namespace Collections
        Type ListError Implements Std.Error
            Operation As Str
        End Type
    End Namespace
End Namespace
";
    let (syms, edges) = run(src);
    let t = syms.iter().find(|s| s.kind == "struct").expect("struct symbol");
    assert_eq!(t.name, "Std.Collections.ListError");

    let e = edges
        .iter()
        .find(|e| e.kind == "implements_interface")
        .expect("implements_interface edge");
    assert_eq!(e.source_name, "Std.Collections.ListError");
    assert_eq!(e.target_name, "Std.Error");
}

#[test]
fn enum_and_interface_declarations() {
    let src = "\
Enum Status
    Idle = 0
    Running = 1
End Enum
Interface IStream
End Interface
";
    let (syms, _) = run(src);
    let e = syms.iter().find(|s| s.kind == "enum").expect("enum symbol");
    assert_eq!(e.name, "Status");
    let m = e.metadata.as_ref().expect("metadata");
    assert_eq!(m.get("members").map(String::as_str), Some("Idle=0||Running=1"));

    let i = syms.iter().find(|s| s.kind == "interface").expect("interface symbol");
    assert_eq!(i.name, "IStream");
}
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p engram_index --test ml_extractor_test`
Expected: FAIL — the five new tests panic with `struct symbol`/`union symbol`/`enum symbol`/`interface symbol` expect messages; the three Task 2 tests still pass.

- [ ] **Step 3: Implement type-body scanning**

Types need their body rows read to classify struct vs union, so they are parsed as a unit rather than purely by the line scanner. In `crates/engram_index/src/ml_extractor/mod.rs`, change the `extract_ml` loop to hand `Type`/`Enum` blocks their body. Replace the `let Some(keyword) = block_opener(trimmed) else { continue; };` block and everything after it inside the loop with:

```rust
        let Some(keyword) = block_opener(trimmed) else {
            continue;
        };

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
        });
```

Add the body collector to `mod.rs`:

```rust
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
```

- [ ] **Step 4: Implement the type declaration arms**

In `crates/engram_index/src/ml_extractor/decls.rs`, change `open_declaration`'s signature to accept the body and add the arms. The new signature:

```rust
pub(crate) fn open_declaration(
    keyword: &str,
    trimmed: &str,
    parent_fqn: &str,
    line_no: u32,
    body: &[&str],
    symbols: &mut Vec<ExtractedSymbol>,
    edges: &mut Vec<ExtractedEdge>,
) -> (String, Option<usize>) {
```

Add these arms before the `_` arm:

```rust
        "Type" => {
            let Some(name) = declaration_name(trimmed, keyword) else {
                return (String::new(), None);
            };
            let fqn = qualify(parent_fqn, &name);

            // A row shaped `Name As Type` is a struct field; `Name(…)` or a
            // bare `Name` is a union variant. Classify by which dominates.
            let mut fields: Vec<String> = Vec::new();
            let mut variants: Vec<String> = Vec::new();
            for row in body {
                if let Some((lhs, rhs)) = row.split_once(" As ") {
                    if !lhs.contains('(') {
                        fields.push(format!("{}:{}", lhs.trim(), rhs.trim()));
                        continue;
                    }
                }
                let vname: String = row
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if vname.is_empty() {
                    continue;
                }
                let arity = match row.split_once('(') {
                    Some((_, args)) => {
                        let args = args.trim_end_matches(')').trim();
                        if args.is_empty() { 0 } else { args.matches(',').count() + 1 }
                    }
                    None => 0,
                };
                variants.push(format!("{vname}/{arity}"));
            }

            let is_union = !variants.is_empty() && fields.is_empty();
            let kind = if is_union { "union" } else { "struct" };

            if let Some(iface) = implements_target(trimmed) {
                edges.push(ExtractedEdge {
                    source_name: fqn.clone(),
                    source_kind: kind.to_string(),
                    source_start_line: line_no,
                    source_language: "ml".to_string(),
                    target_name: iface,
                    target_kind: Some("interface".to_string()),
                    target_start_line: None,
                    kind: "implements_interface".to_string(),
                    metadata: None,
                });
            }

            symbols.push(ExtractedSymbol {
                name: fqn.clone(),
                kind: kind.to_string(),
                start_line: line_no,
                end_line: 0,
                metadata: meta(&[
                    ("fields", fields.join("||")),
                    ("variants", variants.join("||")),
                    ("generic_params", generic_params(trimmed)),
                    ("access", access_modifier(trimmed)),
                ]),
            });
            (fqn, Some(symbols.len() - 1))
        }
        "Enum" => {
            let Some(name) = declaration_name(trimmed, keyword) else {
                return (String::new(), None);
            };
            let fqn = qualify(parent_fqn, &name);
            let members: Vec<String> = body
                .iter()
                .map(|row| row.split_whitespace().collect::<Vec<_>>().join(""))
                .filter(|row| !row.is_empty())
                .collect();
            symbols.push(ExtractedSymbol {
                name: fqn.clone(),
                kind: "enum".to_string(),
                start_line: line_no,
                end_line: 0,
                metadata: meta(&[("members", members.join("||"))]),
            });
            (fqn, Some(symbols.len() - 1))
        }
        "Interface" => {
            let Some(name) = declaration_name(trimmed, keyword) else {
                return (String::new(), None);
            };
            let fqn = qualify(parent_fqn, &name);
            symbols.push(ExtractedSymbol {
                name: fqn.clone(),
                kind: "interface".to_string(),
                start_line: line_no,
                end_line: 0,
                metadata: meta(&[("members", body.join("||"))]),
            });
            (fqn, Some(symbols.len() - 1))
        }
```

Add the helpers to the same file:

```rust
/// Join a parent scope and a local name into an FQN.
pub(crate) fn qualify(parent_fqn: &str, name: &str) -> String {
    if parent_fqn.is_empty() {
        name.to_string()
    } else {
        format!("{parent_fqn}.{name}")
    }
}

/// `Public`/`Private` on a declaration line, or empty.
pub(crate) fn access_modifier(trimmed: &str) -> String {
    for modifier in ["Public", "Private"] {
        if trimmed.starts_with(modifier)
            && trimmed[modifier.len()..].starts_with(' ')
        {
            return modifier.to_string();
        }
    }
    String::new()
}

/// The interface named by an `Implements X` clause, or `None`.
pub(crate) fn implements_target(trimmed: &str) -> Option<String> {
    let (_, rest) = trimmed.split_once(" Implements ")?;
    let name: String = rest
        .trim()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '.')
        .collect();
    if name.is_empty() { None } else { Some(name) }
}

/// Generic parameters from an `Of T As Constraint, U` clause, rendered as
/// `T:Constraint||U`. Empty when the declaration is not generic.
///
/// MiniLang places this clause BETWEEN the declared name and the parameter
/// list, so it must be parsed from the segment starting at ` Of ` and
/// ending at the first `(` that follows it.
pub(crate) fn generic_params(trimmed: &str) -> String {
    let Some(idx) = trimmed.find(" Of ") else {
        return String::new();
    };
    let after = &trimmed[idx + 4..];
    let clause = match after.find('(') {
        Some(p) => &after[..p],
        None => after,
    };
    let clause = match clause.find(" Implements ") {
        Some(p) => &clause[..p],
        None => clause,
    };
    clause
        .split(',')
        .filter_map(|part| {
            let part = part.trim();
            if part.is_empty() {
                return None;
            }
            Some(match part.split_once(" As ") {
                Some((n, c)) => format!("{}:{}", n.trim(), c.trim()),
                None => part.to_string(),
            })
        })
        .collect::<Vec<_>>()
        .join("||")
}
```

- [ ] **Step 5: Update the Namespace and Function arms for the new signature**

In the `"Namespace"` arm, replace the inline FQN construction with `qualify(parent_fqn, name)`. In the `"Function" | "Sub"` arm, replace it with `qualify(parent_fqn, &name)` and extend its metadata:

```rust
                metadata: meta(&[
                    ("is_sub", (keyword == "Sub").to_string()),
                    ("generic_params", generic_params(trimmed)),
                    ("access", access_modifier(trimmed)),
                ]),
```

`body` is empty for `Namespace`/`Function`/`Sub` and goes unused in those arms — no binding or `let _ =` is needed, since it is a shared parameter rather than a per-arm local.

- [ ] **Step 6: Mark strong vs. weak reference fields**

Spec §4 requires strong-`Ref` vs `Weak` field marking so cycle risk (MLC6013) is visible before an edit. In the `"Type"` arm's field loop, replace the field-push line:

```rust
                    if !lhs.contains('(') {
                        fields.push(format!("{}:{}", lhs.trim(), rhs.trim()));
                        continue;
                    }
```

with:

```rust
                    if !lhs.contains('(') {
                        let ty = rhs.trim();
                        // Strong `Ref(Of T)` fields can form ownership
                        // cycles; `Weak(Of T)` is the documented break edge.
                        let strength = if ty.starts_with("Weak(") {
                            "weak"
                        } else if ty.starts_with("Ref(") {
                            "strong"
                        } else {
                            ""
                        };
                        if strength.is_empty() {
                            fields.push(format!("{}:{}", lhs.trim(), ty));
                        } else {
                            fields.push(format!("{}:{}:{}", lhs.trim(), ty, strength));
                        }
                        continue;
                    }
```

Add the covering test to `crates/engram_index/tests/ml_extractor_test.rs`:

```rust
#[test]
fn ref_and_weak_fields_are_marked_by_strength() {
    let src = "\
Type Node
    Parent As Weak(Of Node)
    Child As Ref(Of Node)
    Count As Int
End Type
";
    let (syms, _) = run(src);
    let t = syms.iter().find(|s| s.kind == "struct").expect("struct symbol");
    let m = t.metadata.as_ref().expect("metadata");
    assert_eq!(
        m.get("fields").map(String::as_str),
        Some("Parent:Weak(Of Node):weak||Child:Ref(Of Node):strong||Count:Int")
    );
}
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test -p engram_index --test ml_extractor_test`
Expected: PASS (9 tests)

- [ ] **Step 8: Commit**

```bash
git add crates/engram_index/src/ml_extractor/ crates/engram_index/tests/ml_extractor_test.rs
git commit -m "feat(minilang): struct, union, enum, interface declarations

Type blocks classify as struct (Name As Type rows) or union (variant
rows) from their body. Generic parameters are parsed from the Of clause
between the name and the parameter list, with constraints. Implements
clauses emit implements_interface edges."
```

---

### Task 4: Function signatures, `Throws`, includes, and FFI bindings

**Files:**
- Modify: `crates/engram_index/src/ml_extractor/decls.rs`
- Modify: `crates/engram_index/src/ml_extractor/mod.rs`
- Test: `crates/engram_index/tests/ml_extractor_test.rs`

**Interfaces:**
- Consumes: all Task 2–3 helpers.
- Produces: `pub(crate) fn parse_params(sig: &str) -> Vec<String>`; symbol kind `extern_function`; edge kinds `includes_file`, `dependency` (`relation=throws`).

- [ ] **Step 1: Write the failing tests**

Append to `crates/engram_index/tests/ml_extractor_test.rs`:

```rust
#[test]
fn function_signature_records_params_return_and_throws() {
    let src = "\
Namespace Std
    Function BTreeMap_Get Of K As Ordered, V(Borrow tree As Std.BTreeMap(Of K, V), key As K) As V Throws Std.BTreeLookupError
        Return key
    End Function
End Namespace
";
    let (syms, edges) = run(src);
    let f = syms.iter().find(|s| s.kind == "function").expect("function symbol");
    assert_eq!(f.name, "Std.BTreeMap_Get");
    let m = f.metadata.as_ref().expect("metadata");
    assert_eq!(m.get("generic_params").map(String::as_str), Some("K:Ordered||V"));
    assert_eq!(m.get("params").map(String::as_str), Some("borrow tree||owned key"));
    assert_eq!(m.get("returns").map(String::as_str), Some("V"));
    assert_eq!(m.get("throws").map(String::as_str), Some("Std.BTreeLookupError"));

    let e = edges
        .iter()
        .find(|e| e.kind == "dependency" && e.target_name == "Std.BTreeLookupError")
        .expect("throws edge");
    assert_eq!(e.source_name, "Std.BTreeMap_Get");
    assert_eq!(
        e.metadata.as_ref().and_then(|m| m.get("relation")).map(String::as_str),
        Some("throws")
    );
}

#[test]
fn nullable_return_and_borrowmut_params() {
    let src = "\
Function FirstPositive(BorrowMut buf As Bytes, x As Int) As Int?
    Return x
End Function
";
    let (syms, _) = run(src);
    let f = syms.iter().find(|s| s.kind == "function").expect("function symbol");
    let m = f.metadata.as_ref().expect("metadata");
    assert_eq!(m.get("params").map(String::as_str), Some("borrow_mut buf||owned x"));
    assert_eq!(m.get("returns").map(String::as_str), Some("Int"));
    assert_eq!(m.get("nullable_return").map(String::as_str), Some("true"));
}

#[test]
fn function_type_annotation_is_not_a_declaration() {
    // `Mapper As Function(T) As R` is a FIELD, not a function declaration.
    let src = "\
Type ListMapCursor Of T, R
    Index As Int
    Mapper As Function(T) As R
End Type
";
    let (syms, _) = run(src);
    assert!(
        syms.iter().all(|s| s.kind != "function"),
        "field of function type must not register as a declaration, got {:?}",
        syms.iter().map(|s| (&s.kind, &s.name)).collect::<Vec<_>>()
    );
    let t = syms.iter().find(|s| s.kind == "struct").expect("struct symbol");
    assert_eq!(t.name, "ListMapCursor");
}

#[test]
fn method_convention_links_this_param_to_its_type() {
    let src = "\
Namespace Std
    Type ListError
        Operation As Str
    End Type
    Function Message(this As Std.ListError) As Str
        Return this.Operation
    End Function
End Namespace
";
    let (_, edges) = run(src);
    let e = edges
        .iter()
        .find(|e| e.kind == "contains" && e.target_name == "Std.Message")
        .expect("method containment edge");
    assert_eq!(e.source_name, "Std.ListError");
}

#[test]
fn include_emits_a_file_edge_resolved_relative_to_the_includer() {
    let src = "Include \"Std.Collections.Typed.HashMaps.ml\"\n";
    let (_, edges) = extract_ml(
        Path::new("C:/proj/src/Libraries/Std.Collections.Typed.ml"),
        "src/Libraries/Std.Collections.Typed.ml",
        src,
    );
    let e = edges
        .iter()
        .find(|e| e.kind == "includes_file")
        .expect("includes_file edge");
    assert_eq!(e.target_name, "src/Libraries/Std.Collections.Typed.HashMaps.ml");
    assert_eq!(e.target_kind.as_deref(), Some("file"));
}

#[test]
fn ffi_bindings_record_library_and_binding_style() {
    let src = "\
Unsafe(Ffi)
    Declare Function GetTickCount Lib \"kernel32.dll\" () As Int
    Extern \"C\" Blocking Function SlowOp Lib \"mylib.dll\" (x As Int) As Int
End Unsafe
";
    let (syms, _) = run(src);
    let externs: Vec<&engram_index::parsing::ExtractedSymbol> =
        syms.iter().filter(|s| s.kind == "extern_function").collect();
    assert_eq!(externs.len(), 2, "got {:?}", externs.iter().map(|s| &s.name).collect::<Vec<_>>());

    let tick = externs.iter().find(|s| s.name == "GetTickCount").expect("GetTickCount");
    let m = tick.metadata.as_ref().expect("metadata");
    assert_eq!(m.get("binding").map(String::as_str), Some("pinvoke"));
    assert_eq!(m.get("library").map(String::as_str), Some("kernel32.dll"));

    let slow = externs.iter().find(|s| s.name == "SlowOp").expect("SlowOp");
    let m = slow.metadata.as_ref().expect("metadata");
    assert_eq!(m.get("binding").map(String::as_str), Some("c_ffi"));
    assert_eq!(m.get("library").map(String::as_str), Some("mylib.dll"));
    assert_eq!(m.get("blocking").map(String::as_str), Some("true"));
}
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p engram_index --test ml_extractor_test`
Expected: FAIL — six new tests fail on missing metadata keys and missing symbols/edges.

- [ ] **Step 3: Implement signature parsing**

Add to `crates/engram_index/src/ml_extractor/decls.rs`:

```rust
/// The parenthesised parameter list of a declaration line, rendered as
/// `mode name` entries: `borrow tree||owned key`.
///
/// The list starts at the first `(` that follows the declared name and any
/// `Of …` clause. Nested parens (generic types such as
/// `Std.BTreeMap(Of K, V)`) are balanced, and commas inside them do not
/// split parameters.
pub(crate) fn parse_params(trimmed: &str) -> Vec<String> {
    let Some(open) = param_list_start(trimmed) else {
        return Vec::new();
    };
    let bytes = trimmed.as_bytes();
    let mut depth = 0i32;
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    // `open` is a BYTE index. Filter on the byte offset rather than using
    // `.skip(open)`, which would skip that many CHARS and mis-slice any
    // line containing non-ASCII text.
    for (i, ch) in trimmed.char_indices().filter(|(i, _)| *i >= open) {
        let b = bytes[i];
        if b == b'(' {
            depth += 1;
            if depth == 1 {
                continue;
            }
        } else if b == b')' {
            depth -= 1;
            if depth == 0 {
                break;
            }
        }
        if b == b',' && depth == 1 {
            parts.push(std::mem::take(&mut current));
            continue;
        }
        current.push(ch);
    }
    parts.push(current);

    parts
        .into_iter()
        .filter_map(|p| {
            let p = p.trim();
            if p.is_empty() {
                return None;
            }
            let (mode, rest) = if let Some(r) = p.strip_prefix("BorrowMut ") {
                ("borrow_mut", r)
            } else if let Some(r) = p.strip_prefix("Borrow ") {
                ("borrow", r)
            } else {
                ("owned", p)
            };
            let name: String = rest
                .trim_start()
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if name.is_empty() {
                None
            } else {
                Some(format!("{mode} {name}"))
            }
        })
        .collect()
}

/// Byte index of the `(` that opens the parameter list, skipping any
/// parenthesised generic constraint inside a preceding `Of …` clause.
fn param_list_start(trimmed: &str) -> Option<usize> {
    // Everything before the parameter list is `[access] kw Name [Of …]`.
    // The `Of` clause may itself contain parens, so scan from after it.
    let scan_from = match trimmed.find(" Of ") {
        Some(idx) => {
            // The Of clause ends at the first `(` that is NOT part of a
            // constraint type; constraints in the corpus are bare
            // identifiers, so the first `(` after " Of " opens the params.
            idx + 4
        }
        None => 0,
    };
    trimmed[scan_from..].find('(').map(|p| scan_from + p)
}

/// The `As <Type>` return clause of a Function line, and whether it is
/// nullable (`?`-suffixed). Returns `("", false)` for `Sub` and for
/// functions with no return clause.
pub(crate) fn parse_return(trimmed: &str) -> (String, bool) {
    // The return clause is the LAST ` As ` at paren depth 0 — parameter
    // types also use ` As `, so a naive rsplit would pick up the final
    // parameter's type when there is no return clause.
    let bytes = trimmed.as_bytes();
    let mut depth = 0i32;
    let mut last_as: Option<usize> = None;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b'A' if depth == 0
                && trimmed[i..].starts_with("As ")
                && i > 0
                && bytes[i - 1] == b' ' =>
            {
                last_as = Some(i + 3);
            }
            _ => {}
        }
        i += 1;
    }
    let Some(start) = last_as else {
        return (String::new(), false);
    };
    let rest = trimmed[start..].trim_start();
    let ty: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '.')
        .collect();
    let nullable = rest[ty.len()..].starts_with('?');
    (ty, nullable)
}

/// The error type named by a `Throws E` clause, or empty.
pub(crate) fn parse_throws(trimmed: &str) -> String {
    let Some((_, rest)) = trimmed.split_once(" Throws ") else {
        return String::new();
    };
    rest.trim()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '.')
        .collect()
}
```

- [ ] **Step 4: Extend the Function/Sub arm**

Replace the `"Function" | "Sub"` arm in `open_declaration` with:

```rust
        "Function" | "Sub" => {
            let Some(name) = declaration_name(trimmed, keyword) else {
                return (String::new(), None);
            };
            let fqn = qualify(parent_fqn, &name);
            let params = parse_params(trimmed);
            let (returns, nullable) = if keyword == "Sub" {
                (String::new(), false)
            } else {
                parse_return(trimmed)
            };
            let throws = parse_throws(trimmed);

            if !throws.is_empty() {
                edges.push(ExtractedEdge {
                    source_name: fqn.clone(),
                    source_kind: "function".to_string(),
                    source_start_line: line_no,
                    source_language: "ml".to_string(),
                    target_name: throws.clone(),
                    target_kind: Some("struct".to_string()),
                    target_start_line: None,
                    kind: "dependency".to_string(),
                    metadata: meta(&[("relation", "throws".to_string())]),
                });
            }

            // MiniLang's method convention: a first parameter named `this`
            // makes the function a method of that parameter's type.
            if let Some(first) = params.first() {
                if first.ends_with(" this") {
                    if let Some(owner) = first_param_type(trimmed) {
                        edges.push(ExtractedEdge {
                            source_name: owner,
                            source_kind: "struct".to_string(),
                            source_start_line: 0,
                            source_language: "ml".to_string(),
                            target_name: fqn.clone(),
                            target_kind: Some("function".to_string()),
                            target_start_line: Some(line_no),
                            kind: "contains".to_string(),
                            metadata: meta(&[("relation", "method".to_string())]),
                        });
                    }
                }
            }

            symbols.push(ExtractedSymbol {
                name: fqn.clone(),
                kind: "function".to_string(),
                start_line: line_no,
                end_line: 0,
                metadata: meta(&[
                    ("is_sub", (keyword == "Sub").to_string()),
                    ("generic_params", generic_params(trimmed)),
                    ("access", access_modifier(trimmed)),
                    ("params", params.join("||")),
                    ("returns", returns),
                    ("nullable_return", if nullable { "true".to_string() } else { String::new() }),
                    ("throws", throws),
                ]),
            });
            (fqn, Some(symbols.len() - 1))
        }
```

Add the helper:

```rust
/// The declared type of the first parameter, used to attach `this`-style
/// methods to their owning type.
fn first_param_type(trimmed: &str) -> Option<String> {
    let open = trimmed.find('(')?;
    let rest = &trimmed[open + 1..];
    let (_, after_as) = rest.split_once(" As ")?;
    let ty: String = after_as
        .trim_start()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '.')
        .collect();
    if ty.is_empty() { None } else { Some(ty) }
}
```

- [ ] **Step 5: Implement includes and FFI bindings**

These are single-line constructs, not blocks, so they are handled in the scanner loop rather than `open_declaration`. In `crates/engram_index/src/ml_extractor/mod.rs`, insert this immediately after the `if trimmed.is_empty() { continue; }` guard:

```rust
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
```

Add to `decls.rs`:

```rust
/// The project-relative target of an `Include "…"` line.
///
/// Include paths resolve relative to the INCLUDING file's directory. The
/// result stays project-relative: absolute edge targets are rejected by the
/// ingest safety check.
pub(crate) fn include_target(trimmed: &str, rel_path: &str) -> Option<String> {
    let rest = trimmed.strip_prefix("Include")?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    let raw = rest[..end].trim().replace('\\', "/");
    if raw.is_empty() {
        return None;
    }
    let dir = rel_path.replace('\\', "/");
    let dir = match dir.rfind('/') {
        Some(i) => &dir[..i],
        None => "",
    };
    let joined = if dir.is_empty() {
        raw
    } else {
        format!("{dir}/{raw}")
    };
    // Normalise `a/./b` and `a/b/../c` without touching the filesystem.
    let mut parts: Vec<&str> = Vec::new();
    for seg in joined.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

/// A `Declare Function … Lib "…"` (P/Invoke) or
/// `Extern "C" [Blocking] Function … Lib "…"` (C-FFI) binding.
pub(crate) fn parse_ffi_binding(trimmed: &str, line_no: u32) -> Option<ExtractedSymbol> {
    let (binding, rest) = if let Some(r) = trimmed.strip_prefix("Declare ") {
        ("pinvoke", r)
    } else if let Some(r) = trimmed.strip_prefix("Extern ") {
        // Skip the ABI string: `"C" [Blocking] Function …`.
        let r = r.trim_start();
        let r = r.strip_prefix('"')?;
        let end = r.find('"')?;
        ("c_ffi", &r[end + 1..])
    } else {
        return None;
    };

    let rest = rest.trim_start();
    let (blocking, rest) = match rest.strip_prefix("Blocking ") {
        Some(r) => (true, r.trim_start()),
        None => (false, rest),
    };

    let rest = rest
        .strip_prefix("Function ")
        .or_else(|| rest.strip_prefix("Sub "))?;
    let name: String = rest
        .trim_start()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() {
        return None;
    }

    let library = quoted_after(trimmed, " Lib ").unwrap_or_default();
    let alias = quoted_after(trimmed, " Alias ").unwrap_or_default();

    Some(ExtractedSymbol {
        name,
        kind: "extern_function".to_string(),
        start_line: line_no,
        end_line: line_no,
        metadata: meta(&[
            ("binding", binding.to_string()),
            ("library", library),
            ("alias", alias),
            ("blocking", if blocking { "true".to_string() } else { String::new() }),
            ("params", parse_params(trimmed).join("||")),
        ]),
    })
}

/// The double-quoted string immediately following `marker`.
fn quoted_after(haystack: &str, marker: &str) -> Option<String> {
    let (_, rest) = haystack.split_once(marker)?;
    let rest = rest.trim_start().strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}
```

- [ ] **Step 6: Implement `Const` declarations**

Spec §2 lists `constant` symbols. `Const` is a single-line construct like `Include`, so it is handled in the scanner loop. Add the test to `crates/engram_index/tests/ml_extractor_test.rs`:

```rust
#[test]
fn const_declarations_record_their_ctfe_expression() {
    let src = "\
Namespace Demo
    Const WIDTH = 5 * 2
End Namespace
";
    let (syms, _) = run(src);
    let c = syms.iter().find(|s| s.kind == "constant").expect("constant symbol");
    assert_eq!(c.name, "Demo.WIDTH");
    assert_eq!(
        c.metadata.as_ref().and_then(|m| m.get("value")).map(String::as_str),
        Some("5 * 2")
    );
}
```

Add to `crates/engram_index/src/ml_extractor/decls.rs`:

```rust
/// A `Const NAME = expr` declaration. Constants are CTFE-evaluated and
/// allocate no runtime storage, so the expression text is the useful
/// payload — it is what a fixed-array size resolves to.
pub(crate) fn parse_const(trimmed: &str, parent_fqn: &str, line_no: u32) -> Option<ExtractedSymbol> {
    let rest = trimmed.strip_prefix("Const")?;
    if !rest.starts_with(' ') {
        return None;
    }
    let (name, value) = rest.trim_start().split_once('=')?;
    let name = name.trim();
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    Some(ExtractedSymbol {
        name: qualify(parent_fqn, name),
        kind: "constant".to_string(),
        start_line: line_no,
        end_line: line_no,
        metadata: meta(&[("value", value.trim().to_string())]),
    })
}
```

In `crates/engram_index/src/ml_extractor/mod.rs`, add to the scanner loop immediately after the `parse_ffi_binding` block:

```rust
        {
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
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test -p engram_index --test ml_extractor_test`
Expected: PASS (16 tests)

- [ ] **Step 8: Commit**

```bash
git add crates/engram_index/src/ml_extractor/ crates/engram_index/tests/ml_extractor_test.rs
git commit -m "feat(minilang): signatures, Throws, includes, FFI, constants

Parameter lists balance nested parens so generic types like
BTreeMap(Of K, V) do not split on their internal comma. Return type is
the last ' As ' at paren depth zero -- a naive rsplit picks up the final
parameter's type when a Sub has no return clause. Throws clauses emit
dependency edges to the error type; first-param 'this' attaches the
function to its owning type as a method."
```

---

### Task 5: Call edges, capabilities, concurrency, SIMD, and the module entry symbol

**Files:**
- Modify: `crates/engram_index/src/ml_extractor/bodies.rs`
- Modify: `crates/engram_index/src/ml_extractor/mod.rs`
- Test: `crates/engram_index/tests/ml_extractor_test.rs`

**Interfaces:**
- Consumes: `OpenBlock`, `strip_comment`, `meta`.
- Produces: `pub(crate) fn scan_statement(...)` emitting `calls` edges; the `<module>` synthetic symbol.

- [ ] **Step 1: Write the failing tests**

Append to `crates/engram_index/tests/ml_extractor_test.rs`:

```rust
#[test]
fn call_edges_attribute_to_the_enclosing_function() {
    let src = "\
Namespace Demo
    Function Helper(n As Int) As Int
        Return n
    End Function
    Function Main() As Int
        Return Helper(3)
    End Function
End Namespace
";
    let (_, edges) = run(src);
    let e = edges
        .iter()
        .find(|e| e.kind == "calls" && e.target_name == "Helper")
        .expect("calls edge");
    assert_eq!(e.source_name, "Demo.Main");
}

#[test]
fn spawn_and_detached_are_flagged_on_the_call_edge() {
    let src = "\
Function Worker(n As Int) As Int
    Return n
End Function
Function Boot() As Int
    Spawn Call Worker(42)
    Spawn Detached Hi Call Worker(7)
    Return 0
End Function
";
    let (_, edges) = run(src);
    let spawns: Vec<&engram_index::parsing::ExtractedEdge> = edges
        .iter()
        .filter(|e| e.kind == "calls" && e.target_name == "Worker")
        .collect();
    assert_eq!(spawns.len(), 2, "expected two spawn call edges");

    assert!(spawns.iter().any(|e| {
        let m = e.metadata.as_ref().expect("metadata");
        m.get("spawn").map(String::as_str) == Some("true")
            && m.get("detached").is_none()
    }));
    assert!(spawns.iter().any(|e| {
        let m = e.metadata.as_ref().expect("metadata");
        m.get("detached").map(String::as_str) == Some("true")
            && m.get("priority").map(String::as_str) == Some("Hi")
    }));
}

#[test]
fn channel_and_simd_calls_carry_domain_metadata() {
    let src = "\
Function Pipe() As Int
    Var ch As Channel(Of Int) = NewChannel(Of Int)(4)
    Send(ch, 42)
    Var v As Vector256(Of Int32) = Std.Vector.Splat256 Of Int32(7i32)
    Return 0
End Function
";
    let (_, edges) = run(src);

    let send = edges
        .iter()
        .find(|e| e.kind == "calls" && e.target_name == "Send")
        .expect("Send call edge");
    assert_eq!(
        send.metadata.as_ref().and_then(|m| m.get("concurrency")).map(String::as_str),
        Some("channel")
    );

    let splat = edges
        .iter()
        .find(|e| e.kind == "calls" && e.target_name == "Std.Vector.Splat256")
        .expect("SIMD call edge");
    let m = splat.metadata.as_ref().expect("metadata");
    assert_eq!(m.get("simd_width").map(String::as_str), Some("256"));
    assert_eq!(m.get("lane_type").map(String::as_str), Some("Int32"));
}

#[test]
fn unsafe_capability_block_emits_a_capability_edge() {
    let src = "\
Function Poke(p As Int) As Int
    Unsafe(RawPtr, Alloc)
        Set p^ To 42
    End Unsafe
    Return 0
End Function
";
    let (_, edges) = run(src);
    let e = edges
        .iter()
        .find(|e| e.kind == "dependency" && e.target_name == "Unsafe(RawPtr, Alloc)")
        .expect("capability edge");
    assert_eq!(e.source_name, "Poke");
    let m = e.metadata.as_ref().expect("metadata");
    assert_eq!(m.get("relation").map(String::as_str), Some("capability"));
    assert_eq!(m.get("capabilities").map(String::as_str), Some("RawPtr||Alloc"));
}

#[test]
fn top_level_statements_get_a_module_entry_symbol() {
    let src = "\
Function Fib(n As Int) As Int
    Return n
End Function

Say Fib(15)
";
    let (syms, edges) = run(src);
    let m = syms
        .iter()
        .find(|s| s.name == "Sample.<module>")
        .expect("module entry symbol");
    assert_eq!(m.kind, "function");
    assert_eq!(
        m.metadata.as_ref().and_then(|x| x.get("synthetic")).map(String::as_str),
        Some("module_entry")
    );

    // The top-level call attributes to the module entry, not to nothing.
    let e = edges
        .iter()
        .find(|e| e.kind == "calls" && e.target_name == "Fib")
        .expect("top-level call edge");
    assert_eq!(e.source_name, "Sample.<module>");
}

#[test]
fn pure_declaration_files_get_no_module_entry() {
    let src = "\
Namespace Std
    Function Helper() As Int
        Return 0
    End Function
End Namespace
";
    let (syms, _) = run(src);
    assert!(
        syms.iter().all(|s| !s.name.ends_with("<module>")),
        "stdlib-style declaration-only files must not get a module entry"
    );
}
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p engram_index --test ml_extractor_test`
Expected: FAIL — six new tests fail on missing `calls` edges and the missing module symbol.

- [ ] **Step 3: Implement statement scanning**

Replace the contents of `crates/engram_index/src/ml_extractor/bodies.rs`:

```rust
//! MiniLang statement-level extraction: call sites, capability grants,
//! concurrency, and SIMD.

use crate::parsing::ExtractedEdge;
use regex::Regex;
use std::sync::LazyLock;

use super::decls::meta;

/// A call site: an identifier (optionally dotted, optionally followed by an
/// `Of T` generic argument clause) immediately preceding `(`.
static CALL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b([A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z_][A-Za-z0-9_]*)*)\s*(?:Of\s+([A-Za-z0-9_, ]+?)\s*)?\(")
        .expect("valid MiniLang call regex")
});

/// `Std.Vector.Splat256` / `WithLane128` — the SIMD intrinsic family, whose
/// width is encoded in the name suffix.
static SIMD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^Std\.Vector\.[A-Za-z]+(128|256|512)$").expect("valid SIMD regex"));

/// Keywords that read as calls but are language constructs or control
/// flow — not edges into project code.
const CALL_STOPWORDS: &[&str] = &[
    "if", "elseif", "while", "for", "return", "set", "say", "var", "dim", "mut", "const", "throw",
    "case", "match", "catch", "using", "unsafe", "asm", "type", "function", "sub", "namespace",
    "enum", "interface", "include", "declare", "extern", "spawn", "call", "and", "or", "not",
];

/// Generic TYPE constructors. `Var v As Vector256(Of Int32)` is a type
/// annotation, not a call — without this the graph fills with calls to
/// `Vector256`, `Channel`, and `List` that no function ever makes.
const TYPE_CONSTRUCTORS: &[&str] = &[
    "Channel", "Vector128", "Vector256", "Vector512", "Ref", "Weak", "Atomic", "List", "SoA",
    "Function", "Option", "Result",
];

/// Channel primitives — flagged so concurrency questions are answerable.
const CHANNEL_CALLS: &[&str] = &["Send", "Receive", "Close", "IsClosed", "NewChannel"];

/// Scan one statement line for outgoing edges from `enclosing_fqn`.
pub(crate) fn scan_statement(
    line: &str,
    enclosing_fqn: &str,
    line_no: u32,
    edges: &mut Vec<ExtractedEdge>,
) {
    if enclosing_fqn.is_empty() {
        return;
    }

    // Spawn modifiers precede the call and describe it.
    let mut spawn = false;
    let mut detached = false;
    let mut priority = String::new();
    let mut rest = line;
    if let Some(r) = rest.strip_prefix("Spawn ") {
        spawn = true;
        rest = r.trim_start();
        if let Some(r2) = rest.strip_prefix("Detached ") {
            detached = true;
            rest = r2.trim_start();
        }
        for p in ["Hi ", "Lo ", "Normal "] {
            if let Some(r2) = rest.strip_prefix(p) {
                priority = p.trim().to_string();
                rest = r2.trim_start();
                break;
            }
        }
    }
    let rest = rest.strip_prefix("Call ").map(str::trim_start).unwrap_or(rest);

    for cap in CALL_RE.captures_iter(rest) {
        let Some(m0) = cap.get(1) else {
            continue;
        };
        let name = m0.as_str();
        let last = name.rsplit('.').next().unwrap_or(name);
        if CALL_STOPWORDS.contains(&last.to_ascii_lowercase().as_str()) {
            continue;
        }
        if TYPE_CONSTRUCTORS.contains(&last) {
            continue;
        }
        // A name in type-annotation position (`… As Foo(…)`) declares a
        // type, it does not call anything.
        if rest[..m0.start()].trim_end().ends_with(" As") {
            continue;
        }

        let generic_args = cap.get(2).map(|m| m.as_str().trim().to_string()).unwrap_or_default();

        let mut simd_width = String::new();
        let mut lane_type = String::new();
        if let Some(sc) = SIMD_RE.captures(name) {
            simd_width = sc[1].to_string();
            lane_type = generic_args.clone();
        }

        let concurrency = if CHANNEL_CALLS.contains(&last) {
            "channel".to_string()
        } else {
            String::new()
        };

        edges.push(ExtractedEdge {
            source_name: enclosing_fqn.to_string(),
            source_kind: "function".to_string(),
            source_start_line: 0,
            source_language: "ml".to_string(),
            target_name: name.to_string(),
            target_kind: Some("function".to_string()),
            target_start_line: None,
            kind: "calls".to_string(),
            metadata: meta(&[
                ("spawn", if spawn { "true".to_string() } else { String::new() }),
                ("detached", if detached { "true".to_string() } else { String::new() }),
                ("priority", priority.clone()),
                ("generic_args", generic_args.clone()),
                ("simd_width", simd_width),
                ("lane_type", lane_type),
                ("concurrency", concurrency),
                ("line", line_no.to_string()),
            ]),
        });
    }
}

/// The capability list granted by an `Unsafe(...)` block header.
/// Bare `Unsafe` grants `All`.
pub(crate) fn unsafe_capabilities(trimmed: &str) -> Option<Vec<String>> {
    let rest = trimmed.strip_prefix("Unsafe")?;
    let rest = rest.trim();
    if rest.is_empty() {
        return Some(vec!["All".to_string()]);
    }
    let inner = rest.strip_prefix('(')?;
    let end = inner.find(')')?;
    Some(
        inner[..end]
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
    )
}
```

- [ ] **Step 4: Wire statement scanning and the module entry into the scanner**

In `crates/engram_index/src/ml_extractor/mod.rs`, inside the loop, after the `block_closer` handling and before the `block_opener` check, add:

```rust
        // Statement lines (everything that opens no block) contribute call
        // edges attributed to the innermost enclosing function.
        if block_opener(trimmed).is_none() {
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
```

Declare `let mut top_level_lines: Vec<(u32, String)> = Vec::new();` alongside the other locals at the top of `extract_ml`.

Add the capability edge to the `block_opener` handling, immediately after `let Some(keyword) = block_opener(trimmed) else { continue; };`:

```rust
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
```

After the loop, before the unterminated-block cleanup, add the module entry:

```rust
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
```

- [ ] **Step 5: Record local binding modes and fallible regions**

Spec §4 requires local mutability (`Dim` immutable vs `Var`/`Mut` mutable) and `Try`/`Catch`/`Finally` regions on the function symbol, so `check_edit_safety` can warn before an edit. These are body facts, so they are collected while scanning and backfilled onto the function symbol when its block closes.

Add the test to `crates/engram_index/tests/ml_extractor_test.rs`:

```rust
#[test]
fn function_records_local_binding_modes_and_fallible_regions() {
    let src = "\
Function Risky() As Int
    Dim fixed As Int
    Var counter As Int
    Mut total As Int
    Try
        Set total To counter
    Catch
        Set total To 0
    End Try
    Return total
End Function
";
    let (syms, _) = run(src);
    let f = syms.iter().find(|s| s.kind == "function").expect("function symbol");
    let m = f.metadata.as_ref().expect("metadata");
    assert_eq!(m.get("immutable_locals").map(String::as_str), Some("fixed"));
    assert_eq!(m.get("mutable_locals").map(String::as_str), Some("counter||total"));
    assert_eq!(m.get("has_catch").map(String::as_str), Some("true"));
}
```

Add to `crates/engram_index/src/ml_extractor/bodies.rs`:

```rust
/// A local binding declaration: `Dim x As Int` (immutable) or
/// `Var`/`Mut x As Int` (mutable). Returns `(name, is_mutable)`.
pub(crate) fn local_binding(trimmed: &str) -> Option<(String, bool)> {
    for (kw, mutable) in [("Dim ", false), ("Var ", true), ("Mut ", true)] {
        if let Some(rest) = trimmed.strip_prefix(kw) {
            let name: String = rest
                .trim_start()
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                return Some((name, mutable));
            }
        }
    }
    None
}
```

In `crates/engram_index/src/ml_extractor/mod.rs`, extend `OpenBlock` with three collection fields:

```rust
    /// Locals declared directly in this block, for ownership metadata.
    pub immutable_locals: Vec<String>,
    pub mutable_locals: Vec<String>,
    /// True once a `Catch` line is seen inside this block.
    pub has_catch: bool,
```

Initialise them to `Vec::new()` / `false` at every `stack.push(OpenBlock { … })` site.

In the statement branch, insert this as a standalone block **before** the `match enclosing { … }` — it needs its own mutable borrow of the stack, which is why it cannot live inside the match arm that already holds the cloned FQN:

```rust
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
```

In the `block_closer` branch, backfill before the symbol's `end_line` is set:

```rust
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
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p engram_index --test ml_extractor_test`
Expected: PASS (23 tests)

- [ ] **Step 7: Commit**

```bash
git add crates/engram_index/src/ml_extractor/ crates/engram_index/tests/ml_extractor_test.rs
git commit -m "feat(minilang): call edges, capabilities, concurrency, SIMD

Call sites attribute to the innermost enclosing Function/Sub. Spawn
modifiers, channel primitives, and Std.Vector SIMD intrinsics carry
domain metadata. Unsafe(...) blocks emit capability edges. Script-style
files get a synthetic <module> entry so top-level calls have a caller
instead of dangling."
```

---

### Task 6: `Ui` DSL and inline `Asm`

**Files:**
- Modify: `crates/engram_index/src/ml_extractor/ui.rs`
- Modify: `crates/engram_index/src/ml_extractor/mod.rs`
- Test: `crates/engram_index/tests/ml_extractor_test.rs`

**Interfaces:**
- Consumes: `OpenBlock`, `meta`.
- Produces: symbol kinds `ui_container`, `control`, `inline_asm`; `contains_ui` edges.

- [ ] **Step 1: Write the failing tests**

Append to `crates/engram_index/tests/ml_extractor_test.rs`:

```rust
#[test]
fn ui_block_produces_nested_containers_and_controls() {
    let src = "\
Ui Width 420 Height 160 Bg bg
  Panel
    Bg surface
    Rect 20 20 400 140 16
    Label
      Text \"Deployment status\"
      Rect 44 38 360 70 0
    End Label
    Badge
      Text \"Active\"
      Rect 44 84 134 112 0
    End Badge
  End Panel
End Ui
";
    let (syms, edges) = run(src);

    let root = syms
        .iter()
        .find(|s| s.kind == "ui_container" && s.name == "Sample.Ui")
        .expect("root Ui container");
    let m = root.metadata.as_ref().expect("metadata");
    assert_eq!(m.get("width").map(String::as_str), Some("420"));
    assert_eq!(m.get("height").map(String::as_str), Some("160"));

    let label = syms
        .iter()
        .find(|s| s.kind == "control" && s.name.ends_with(".Label"))
        .expect("Label control");
    let lm = label.metadata.as_ref().expect("metadata");
    assert_eq!(lm.get("text").map(String::as_str), Some("Deployment status"));
    assert_eq!(lm.get("rect").map(String::as_str), Some("44 38 360 70 0"));
    assert_eq!(lm.get("element").map(String::as_str), Some("Label"));

    // Nesting is expressed as contains_ui edges parent -> child.
    assert!(
        edges.iter().any(|e| e.kind == "contains_ui"
            && e.source_name == "Sample.Ui"
            && e.target_name.ends_with(".Panel")),
        "Ui should contain Panel"
    );
    assert!(
        edges.iter().any(|e| e.kind == "contains_ui"
            && e.source_name.ends_with(".Panel")
            && e.target_name.ends_with(".Label")),
        "Panel should contain Label"
    );
}

#[test]
fn inline_asm_block_records_mnemonics_and_bindings() {
    let src = "\
Function Fast(x As Int) As Int
    Asm
        In x As Int
        Mov Rax, Rbx
        Add Rax, 1
        Out result As Int
    End Asm
    Return x
End Function
";
    let (syms, _) = run(src);
    let a = syms.iter().find(|s| s.kind == "inline_asm").expect("inline_asm symbol");
    let m = a.metadata.as_ref().expect("metadata");
    assert_eq!(m.get("mnemonics").map(String::as_str), Some("Mov||Add"));
    assert_eq!(m.get("inputs").map(String::as_str), Some("x:Int"));
    assert_eq!(m.get("outputs").map(String::as_str), Some("result:Int"));
    assert_eq!(m.get("owner").map(String::as_str), Some("Fast"));
}
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p engram_index --test ml_extractor_test`
Expected: FAIL — both tests fail on missing `ui_container`/`inline_asm` symbols.

- [ ] **Step 3: Implement the UI and Asm extractors**

Replace the contents of `crates/engram_index/src/ml_extractor/ui.rs`:

```rust
//! MiniLang declarative `Ui` DSL and inline `Asm` blocks.

use crate::parsing::ExtractedSymbol;

use super::decls::meta;

/// UI elements that open a nesting level and become graph nodes. `Ui` is
/// the root container; the rest are controls.
pub(crate) const UI_ELEMENTS: &[&str] = &[
    "Ui", "Panel", "Label", "Button", "Badge", "Card", "Field", "Checkbox", "Radio", "Slider",
    "ProgressBar", "Image", "Divider", "VStack",
];

/// Attribute rows inside a UI element — they configure the enclosing
/// element rather than opening a nesting level.
pub(crate) fn ui_attribute(trimmed: &str) -> Option<(String, String)> {
    for key in ["Rect", "Bg", "Text", "Style", "Border", "Gradient", "Shadow"] {
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
/// through the element's ancestors, which keeps sibling elements of the
/// same type distinct.
pub(crate) fn ui_symbol(element: &str, fqn: &str, line_no: u32) -> ExtractedSymbol {
    let kind = if element == "Ui" { "ui_container" } else { "control" };
    ExtractedSymbol {
        name: fqn.to_string(),
        kind: kind.to_string(),
        start_line: line_no,
        end_line: 0,
        metadata: meta(&[("element", element.to_string())]),
    }
}

/// Header attributes on the `Ui` line itself: `Ui Width 420 Height 160 Bg bg`.
pub(crate) fn ui_header_attrs(trimmed: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    let mut i = 1usize; // skip the `Ui` keyword
    while i + 1 < tokens.len() {
        out.push((tokens[i].to_ascii_lowercase(), tokens[i + 1].to_string()));
        i += 2;
    }
    out
}

/// Parse an inline `Asm` block body into mnemonics and In/Out bindings.
pub(crate) fn asm_symbol(
    body: &[&str],
    owner: &str,
    line_no: u32,
    end_line: u32,
) -> ExtractedSymbol {
    let mut mnemonics: Vec<String> = Vec::new();
    let mut inputs: Vec<String> = Vec::new();
    let mut outputs: Vec<String> = Vec::new();

    for row in body {
        if let Some(rest) = row.strip_prefix("In ") {
            if let Some((n, t)) = rest.split_once(" As ") {
                inputs.push(format!("{}:{}", n.trim(), t.trim()));
            }
            continue;
        }
        if let Some(rest) = row.strip_prefix("Out ") {
            if let Some((n, t)) = rest.split_once(" As ") {
                outputs.push(format!("{}:{}", n.trim(), t.trim()));
            }
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
        end_line,
        metadata: meta(&[
            ("owner", owner.to_string()),
            ("mnemonics", mnemonics.join("||")),
            ("inputs", inputs.join("||")),
            ("outputs", outputs.join("||")),
        ]),
    }
}
```

- [ ] **Step 4: Wire UI and Asm into the scanner**

In `crates/engram_index/src/ml_extractor/mod.rs`, replace the UI element entries in `BLOCK_KEYWORDS` — remove the bogus `"Switch2"` placeholder and source the list from `ui::UI_ELEMENTS` instead. Change the constant to:

```rust
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
    // UI DSL — see `ui::UI_ELEMENTS`.
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
];
```

In the block-opening branch of the loop, before the `decls::open_declaration` call, add UI and Asm handling:

```rust
        if ui::UI_ELEMENTS.contains(&keyword) {
            let stem = file_stem(rel_path);
            let parent = stack
                .iter()
                .rev()
                .find(|b| ui::UI_ELEMENTS.contains(&b.keyword.as_str()))
                .map(|b| b.fqn.clone());
            let fqn = match &parent {
                Some(p) => format!("{p}.{keyword}"),
                None => format!("{stem}.{keyword}"),
            };
            let mut sym = ui::ui_symbol(keyword, &fqn, line_no);
            if keyword == "Ui" {
                let mut m = sym.metadata.take().unwrap_or_default();
                for (k, v) in ui::ui_header_attrs(trimmed) {
                    m.insert(k, v);
                }
                sym.metadata = Some(m);
            }
            // Attribute rows inside this element configure it.
            let body = collect_block_body(source, idx, keyword);
            if let Some(m) = sym.metadata.as_mut() {
                for row in &body {
                    if let Some((k, v)) = ui::ui_attribute(row) {
                        m.entry(k).or_insert(v);
                    }
                }
            }
            symbols.push(sym);
            let symbol_idx = Some(symbols.len() - 1);

            if let Some(p) = parent {
                edges.push(ExtractedEdge {
                    source_name: p,
                    source_kind: "ui_container".to_string(),
                    source_start_line: 0,
                    source_language: "ml".to_string(),
                    target_name: fqn.clone(),
                    target_kind: Some("control".to_string()),
                    target_start_line: Some(line_no),
                    kind: "contains_ui".to_string(),
                    metadata: None,
                });
            }

            stack.push(OpenBlock {
                keyword: keyword.to_string(),
                fqn,
                start_line: line_no,
                symbol_idx,
            });
            continue;
        }

        if keyword == "Asm" {
            let owner = stack
                .iter()
                .rev()
                .find(|b| b.keyword == "Function" || b.keyword == "Sub")
                .map(|b| b.fqn.clone())
                .unwrap_or_else(|| file_stem(rel_path));
            let body = collect_block_body(source, idx, "Asm");
            let end_line = line_no + body.len() as u32 + 1;
            symbols.push(ui::asm_symbol(&body, &owner, line_no, end_line));
            stack.push(OpenBlock {
                keyword: keyword.to_string(),
                fqn: String::new(),
                start_line: line_no,
                symbol_idx: None,
            });
            continue;
        }
```

Add the helper to `mod.rs`:

```rust
/// File stem of a project-relative path: `src/Lib/Badge.ml` → `Badge`.
pub(crate) fn file_stem(rel_path: &str) -> String {
    rel_path
        .rsplit(['/', '\\'])
        .next()
        .and_then(|f| f.split('.').next())
        .unwrap_or("module")
        .to_string()
}
```

Replace the inline stem computation in the module-entry block with `file_stem(rel_path)`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p engram_index --test ml_extractor_test`
Expected: PASS (25 tests)

- [ ] **Step 6: Commit**

```bash
git add crates/engram_index/src/ml_extractor/ crates/engram_index/tests/ml_extractor_test.rs
git commit -m "feat(minilang): Ui DSL containers and inline Asm

UI elements nest into ui_container/control nodes joined by contains_ui
edges; attribute rows (Rect, Bg, Text, Style) fold into the element's
metadata. Inline Asm blocks record mnemonics and In/Out bindings against
their owning function."
```

---

### Task 6b: Scanner keyword completeness (retrospective — no section written at plan time)

**This section did not exist when the plan was originally executed.** Task 6b was a substantial,
separately-reviewed unit of work discovered during Task 6's own review, and the plan was never
updated to record it — added here after the fact so the plan remains an accurate durable record.
Task 6 has otherwise not been re-scoped or re-ordered; this section documents work that already
happened, between Task 6 and Task 7.

**Why it existed:** Task 6's review measured the extractor against the real corpus and found 276
stack desyncs across 34 files, traced to four block keywords entirely missing from
`BLOCK_KEYWORDS` (`Repeat`, `Union`, `Func`, `For`), plus an `Asm`/`Sub` mnemonic collision and a
field access-modifier leaking into field names. None of this was a regression from Task 6's own
changes — it was pre-existing scanner incompleteness that Task 6's corpus-measurement pass was
simply the first to quantify.

**Files:**
- `crates/engram_index/src/ml_extractor/decls.rs`
- `crates/engram_index/src/ml_extractor/mod.rs`
- `crates/engram_index/src/ml_extractor/bodies.rs`
- `crates/engram_index/tests/ml_extractor_test.rs`

**What it covered, in two commits:**

- **`cacb179` — scanner keyword completeness.** Added `Union` as an explicit tagged-union
  declaration (`decls.rs`, sharing a new `parse_variant` helper with `Type`'s implicit-union
  fallback), `Repeat` for balance only (no symbol), and `Func` — MiniLang's alternate
  `Func Name(...) -> Type ... End Func` declaration syntax, via a new `parse_arrow_return` helper
  and `is_function_like()` unifying `Function`/`Sub`/`Func` across the extractor's call sites.
  Fixed `For`/`Next` balancing (`For` opens, closed by the bare word `Next` via `closes_block`, or
  `End For` in compiler-rejected fixtures), which incidentally surfaced and fixed a `Parallel For`
  (MIMD loop) regression the corpus measurement pass itself caught before commit. Also fixed the
  `Asm`/`Sub` mnemonic collision (`Sub Rbx, Rax` operand rows no longer fabricate a phantom `Sub`
  declaration inside an `Asm` block — `skip_as_member_row`) and a field access-modifier leak
  (`Public`/`Private` were not being stripped from field names in `Type` field-row parsing).
  57/57 tests passing (48 existing + 9 new). Of the 280 baseline desyncs, the 20 attributable to
  these six items dropped to 0; 262 remained, ~98% traced to a separate, then-out-of-scope
  `Try`/`Try Call` collision.
- **`660930d` — Try/Try-Call collision + review round-1 fixes.** The `Try`/`Try Call` collision,
  initially flagged out of scope, turned out to be ~98% of the round-0 residual (256 of 262
  desyncs) and was pulled into scope: `block_opener` had treated bare `Try` and `Try Call X(...)`
  identically, but only bare `Try` opens a real `Try...End Try` block — `Try Call X(...)` (187
  corpus occurrences) is a single-line fallible-call statement with no `End Try` of its own. `Try`
  is now special-cased to require the entire line be bare; the statement still falls through to
  normal call-edge extraction. Every other `BLOCK_KEYWORDS` entry was checked for the same
  bare-vs-prefix duality (`If`/`While`/`Select` have bare occurrences, but only in deliberately
  malformed negative/fuzz fixtures; `Unsafe`/`Using` always open) — `Try` was the only real case.
  Corpus measurement: 280 → 262 (round 0) → 20 (round 1), a 92.9% reduction from baseline. The
  remaining 20 split into 6 pre-existing/expected (fuzz + negative fixtures + a non-standard draft
  dialect) and 14 from two newly-discovered, unrelated, unfixed constructs the line-based scanner
  doesn't model (single-line `If cond Then stmt`; single-line `Function ... End Function` on one
  physical line) — flagged for a future round, not fixed here. This commit also re-captured RED
  honestly against the true pre-Task-6b baseline, corrected two factual errors in the round-0
  report, fixed a stale doc comment on `member_shaped`, switched `parse_arrow_return` to
  `rsplit_once`, and added two `closes_block` safety-property regression tests plus a `Try`-`Call`
  regression test. 60/60 tests passing (57 + 3 new).

---

### Task 7: `EdgeKind::TestOracle` and conformance-golden pairing

**Files:**
- Modify: `crates/engram_graph/src/store.rs`
- Modify: `crates/engram_server/src/services/ingest_service.rs`
- Modify: `crates/engram_index/src/ml_extractor/mod.rs`
- Test: `crates/engram_index/tests/ml_extractor_test.rs`, inline test in `store.rs`

**Interfaces:**
- Consumes: `extract_ml`'s `abs_path` parameter.
- Produces: `EdgeKind::TestOracle` (wire string `"test_oracle"`); `test_oracle` edges from `.ml` to golden siblings.

- [ ] **Step 1: Write the failing tests**

Append to `crates/engram_index/tests/ml_extractor_test.rs`:

```rust
#[test]
fn golden_siblings_that_exist_produce_oracle_edges() {
    // The pairing stats the disk, so build a real temp layout.
    let dir = std::env::temp_dir().join("engram_ml_oracle_test");
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let ml = dir.join("abi_deep_recursion.ml");
    std::fs::write(&ml, "Say 1\n").expect("write ml");
    std::fs::write(dir.join("abi_deep_recursion.expected"), "610\n").expect("write expected");

    let (_, edges) = extract_ml(&ml, "tests/conformance/abi/abi_deep_recursion.ml", "Say 1\n");

    let e = edges
        .iter()
        .find(|e| e.kind == "test_oracle")
        .expect("test_oracle edge");
    assert_eq!(e.source_name, "tests/conformance/abi/abi_deep_recursion.ml");
    assert_eq!(e.target_name, "tests/conformance/abi/abi_deep_recursion.expected");
    assert_eq!(e.target_kind.as_deref(), Some("file"));
    assert_eq!(
        e.metadata.as_ref().and_then(|m| m.get("oracle")).map(String::as_str),
        Some("expected")
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn missing_golden_siblings_produce_no_phantom_edges() {
    let dir = std::env::temp_dir().join("engram_ml_oracle_none_test");
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let ml = dir.join("lonely.ml");
    std::fs::write(&ml, "Say 1\n").expect("write ml");

    let (_, edges) = extract_ml(&ml, "tests/lonely.ml", "Say 1\n");
    assert!(
        edges.iter().all(|e| e.kind != "test_oracle"),
        "a .ml file with no golden must not mint a phantom oracle target"
    );

    std::fs::remove_dir_all(&dir).ok();
}
```

Append to the `#[cfg(test)] mod tests` block in `crates/engram_graph/src/store.rs`:

```rust
#[test]
fn test_oracle_edge_kind_round_trips() {
    assert_eq!(EdgeKind::TestOracle.as_str(), "test_oracle");
    assert_eq!(EdgeKind::parse("test_oracle"), Some(EdgeKind::TestOracle));
    assert!(
        EdgeKind::ALL.contains(&EdgeKind::TestOracle),
        "TestOracle must be in ALL or count-by-kind reporting silently omits it"
    );
}
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p engram_graph test_oracle_edge_kind_round_trips && cargo test -p engram_index --test ml_extractor_test golden`
Expected: FAIL to compile — `no variant named \`TestOracle\``

- [ ] **Step 3: Add the edge kind**

In `crates/engram_graph/src/store.rs`, add to the `EdgeKind` enum after `Implements`:

```rust
    /// A conformance-test source file paired with its golden output sidecar
    /// (`foo.ml` → `foo.expected` / `foo.error`).
    TestOracle,
```

Add to `EdgeKind::ALL`:

```rust
        EdgeKind::TestOracle,
```

Add to `as_str`:

```rust
            EdgeKind::TestOracle => "test_oracle",
```

Add to `parse`:

```rust
            "test_oracle" => Some(EdgeKind::TestOracle),
```

In `crates/engram_server/src/services/ingest_service.rs`, add to the `raw_kind` match:

```rust
            "test_oracle" => engram_graph::EdgeKind::TestOracle,
```

- [ ] **Step 4: Implement golden pairing**

In `crates/engram_index/src/ml_extractor/mod.rs`, replace `let _ = abs_path;` with a call at the end of `extract_ml`, just before the return:

```rust
    emit_golden_oracle_edges(abs_path, rel_path, &mut edges);
```

Add the function:

```rust
/// Link a conformance-test source to its golden sidecars.
///
/// The sibling is stat'd before an edge is emitted. Emitting purely from
/// the naming convention would mint phantom file targets for the thousands
/// of `.ml` files that have no golden.
fn emit_golden_oracle_edges(abs_path: &Path, rel_path: &str, edges: &mut Vec<ExtractedEdge>) {
    const ORACLES: &[&str] = &["expected", "error", "exitcode"];
    let rel = rel_path.replace('\\', "/");
    let Some(rel_stem) = rel.strip_suffix(".ml").or_else(|| rel.strip_suffix(".mlinc")) else {
        return;
    };
    for oracle in ORACLES {
        let sibling = abs_path.with_extension(oracle);
        if !sibling.is_file() {
            continue;
        }
        edges.push(ExtractedEdge {
            source_name: rel.clone(),
            source_kind: "file".to_string(),
            source_start_line: 0,
            source_language: "ml".to_string(),
            target_name: format!("{rel_stem}.{oracle}"),
            target_kind: Some("file".to_string()),
            target_start_line: None,
            kind: "test_oracle".to_string(),
            metadata: decls::meta(&[("oracle", (*oracle).to_string())]),
        });
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p engram_graph test_oracle_edge_kind_round_trips && cargo test -p engram_index --test ml_extractor_test`
Expected: PASS (1 + 27 tests)

- [ ] **Step 6: Commit**

```bash
git add crates/engram_graph/src/store.rs crates/engram_server/src/services/ingest_service.rs crates/engram_index/src/ml_extractor/ crates/engram_index/tests/ml_extractor_test.rs
git commit -m "feat(minilang): test_oracle edge for conformance goldens

Pairs foo.ml with its foo.expected/.error/.exitcode sidecars so
test-discovery can answer what proves a feature works. Siblings are
stat'd before emitting -- naming-convention-only emission would mint
phantom targets for the thousands of .ml files without goldens."
```

---

### Task 8: Wire the extractor into indexing

**Files:**
- Modify: `crates/engram_index/src/hybrid.rs` (~line 1619)
- Test: `crates/engram_index/tests/ml_corpus_smoke_test.rs`

**Interfaces:**
- Consumes: `extract_ml` from Task 2–7.
- Produces: `.ml`/`.mlinc` files yield symbols and edges during a real index run.

- [ ] **Step 1: Write the failing smoke test**

Create `crates/engram_index/tests/ml_corpus_smoke_test.rs`:

```rust
//! Regression floor for the MiniLang extractor over a representative
//! composite of shapes drawn from the MiniLang standard library.

use engram_index::ml_extractor::extract_ml;
use std::path::Path;

const CORPUS: &str = r#"
' Representative MiniLang shapes: namespaces, generics, ADTs, methods,
' interfaces, FFI, concurrency, and a UI block.
Namespace Std
    Namespace Collections
        Public Type MapEntry Of K, V
            Key As K
            Value As V
        End Type

        Type ListError Implements Std.Error
            Operation As Str
            Index As Int
        End Type

        Function Message(this As Std.Collections.ListError) As Str
            Return this.Operation
        End Function

        Sub BTreeMap_Install Of K, V(BorrowMut target As Std.Collections.BTreeMap(Of K, V), Borrow replacement As Std.Collections.BTreeMap(Of K, V))
            Call Std.Memory.Copy(target, replacement)
        End Sub

        Function BTreeMap_Get Of K As Ordered, V(Borrow tree As Std.Collections.BTreeMap(Of K, V), key As K) As V Throws Std.Collections.BTreeLookupError
            Return Std.Collections.BTreeCore_Lookup Of K, V(tree, key)
        End Function
    End Namespace

    Interface Error
    End Interface
End Namespace

Type Shape
    Circle(radius As Int)
    Rectangle(w As Int, h As Int)
    Point
End Type

Enum Status
    Idle = 0
    Running = 1
End Enum

Unsafe(Ffi)
    Declare Function GetTickCount Lib "kernel32.dll" () As Int
End Unsafe

Function Producer(Borrow out As Channel(Of Int)) As Int
    Var i As Int
    For i = 1 To 5
        Send(out, i * i)
    Next
    Close(out)
    Return 0
End Function

Function Boot() As Int
    Unsafe(RawPtr)
        Say 1
    End Unsafe
    Spawn Detached Call Producer(0)
    Return 0
End Function

Ui Width 360 Height 184 Bg bg
  Panel
    Label
      Text "Ready"
    End Label
  End Panel
End Ui

Say Boot()
"#;

#[test]
fn corpus_composite_extracts_every_construct_family() {
    let (syms, edges) = extract_ml(
        Path::new("C:/proj/src/Corpus.ml"),
        "src/Corpus.ml",
        CORPUS,
    );

    let count = |kind: &str| syms.iter().filter(|s| s.kind == kind).count();

    // Namespaces: Std, Std.Collections.
    assert_eq!(count("namespace"), 2, "namespaces");
    // Message, BTreeMap_Install, BTreeMap_Get, Producer, Boot, <module>.
    assert_eq!(count("function"), 6, "functions");
    // MapEntry, ListError.
    assert_eq!(count("struct"), 2, "structs");
    assert_eq!(count("union"), 1, "unions (Shape)");
    assert_eq!(count("enum"), 1, "enums");
    assert_eq!(count("interface"), 1, "interfaces");
    assert_eq!(count("extern_function"), 1, "extern functions");
    assert_eq!(count("ui_container"), 1, "ui containers");
    assert_eq!(count("control"), 2, "ui controls (Panel, Label)");

    let ekind = |k: &str| edges.iter().filter(|e| e.kind == k).count();
    assert_eq!(ekind("implements_interface"), 1, "implements edges");
    assert_eq!(ekind("contains_ui"), 2, "contains_ui edges");
    assert!(ekind("calls") >= 5, "calls edges, got {}", ekind("calls"));

    // The Throws clause on BTreeMap_Get.
    assert!(
        edges.iter().any(|e| e.kind == "dependency"
            && e.target_name == "Std.Collections.BTreeLookupError"
            && e.metadata.as_ref().and_then(|m| m.get("relation")).map(String::as_str)
                == Some("throws")),
        "throws edge"
    );

    // Generic Sub declaration must be found despite the Of clause.
    assert!(
        syms.iter().any(|s| s.name == "Std.Collections.BTreeMap_Install"),
        "generic Sub declaration must be extracted"
    );
}
```

- [ ] **Step 2: Run it to make sure it fails or passes**

Run: `cargo test -p engram_index --test ml_corpus_smoke_test`
Expected: PASS if Tasks 2–7 are complete and correct. If any count is off, fix the extractor — this test is the regression floor, and a wrong count here means a construct family is silently mis-parsed.

- [ ] **Step 3: Add the dispatch arm**

In `crates/engram_index/src/hybrid.rs`, add an arm to the extractor chain immediately after the `Some("vb")` arm (around line 1619):

```rust
                        } else if matches!(ext_lower.as_deref(), Some("ml" | "mlinc")) {
                            // MiniLang. Takes BOTH paths: the absolute one to
                            // stat conformance-golden siblings, the
                            // project-relative one to build edge targets —
                            // absolute edge targets are rejected by the
                            // ingest safety check.
                            crate::ml_extractor::extract_ml(p, arc_rel.as_str(), &text)
```

- [ ] **Step 4: Verify the workspace compiles and the suite is green**

Run: `cargo fmt --all && cargo check --all-targets && cargo test -p engram_index --tests --no-fail-fast`
Expected: clean build, all `engram_index` tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/engram_index/src/hybrid.rs crates/engram_index/tests/ml_corpus_smoke_test.rs
git commit -m "feat(minilang): dispatch .ml/.mlinc to the MiniLang extractor

Adds the hybrid.rs ingest arm plus a corpus smoke test that pins one
count per construct family, so a silently mis-parsed family fails
loudly instead of shrinking the graph."
```

---

### Task 9: Method-body extraction parity

Brings `get_full_method_body`, `analyze_business_logic`, and `get_method_edit_context` to VB parity. `.ml` currently resolves to `"vb"` by accident via the `End Function` content sniff.

**Files:**
- Modify: `crates/engram_server/src/services/business_logic_service.rs` (`detect_language` ~line 615, `extract_method_names` ~line 940, regex statics ~line 930)
- Modify: `crates/engram_server/src/services/full_project_migration_service.rs` (add `extract_ml_method_body` near `extract_vb_method_body` ~line 1165)
- Test: inline `#[cfg(test)]` in both files

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `pub(crate) fn extract_ml_method_body(content: &str, method_name: &str) -> Option<(String, u32, u32, u32)>` — same tuple shape as `extract_vb_method_body`: `(body, start_line, end_line, line_count)`.

- [ ] **Step 1: Write the failing tests**

Append to the `#[cfg(test)] mod tests` block in `crates/engram_server/src/services/full_project_migration_service.rs`:

```rust
#[test]
fn extract_ml_method_body_handles_generics_and_nested_blocks() {
    let src = "\
Namespace Std
    Function BTreeMap_Get Of K As Ordered, V(Borrow tree As Int, key As K) As V Throws Std.LookupError
        If key > 0
            Unsafe(RawPtr)
                Say 1
            End Unsafe
        End If
        Return key
    End Function

    Function After() As Int
        Return 1
    End Function
End Namespace
";
    let (body, start, end, lines) =
        extract_ml_method_body(src, "BTreeMap_Get").expect("body extracted");

    assert!(body.starts_with("    Function BTreeMap_Get Of K As Ordered"));
    assert!(body.trim_end().ends_with("End Function"));
    // Must not run past its own End Function into the next declaration.
    assert!(!body.contains("After"), "body leaked into the next function");
    assert_eq!(start, 2);
    assert_eq!(end, 9);
    assert_eq!(lines, 8);
}

#[test]
fn extract_ml_method_body_handles_sub_and_access_modifiers() {
    let src = "\
Public Sub Install(target As Int)
    Say target
End Sub
";
    let (body, _, _, _) = extract_ml_method_body(src, "Install").expect("body extracted");
    assert!(body.contains("Public Sub Install"));
    assert!(body.trim_end().ends_with("End Sub"));
}

#[test]
fn extract_ml_method_body_ignores_function_type_annotations() {
    // `Mapper As Function(T) As R` must not be mistaken for a declaration
    // of a function named Mapper.
    let src = "\
Type Cursor Of T, R
    Mapper As Function(T) As R
End Type
Function Mapper(x As Int) As Int
    Return x
End Function
";
    let (body, start, _, _) = extract_ml_method_body(src, "Mapper").expect("body extracted");
    assert_eq!(start, 4, "must find the real declaration, not the field row");
    assert!(body.starts_with("Function Mapper(x As Int)"));
}
```

Append to the `#[cfg(test)] mod tests` block in `crates/engram_server/src/services/business_logic_service.rs`:

```rust
#[test]
fn detect_language_recognises_minilang() {
    assert_eq!(detect_language("Std.Collections.List.ml", ""), "ml");
    assert_eq!(detect_language("shared.mlinc", ""), "ml");
    // VB and C# are unaffected.
    assert_eq!(detect_language("Form1.vb", ""), "vb");
    assert_eq!(detect_language("Program.cs", ""), "cs");
}

#[test]
fn extract_method_names_finds_minilang_declarations() {
    let src = "\
Namespace Std
    Function BTreeMap_Get Of K, V(tree As Int, key As K) As V
        Return key
    End Function
    Public Sub Install(target As Int)
        Say target
    End Sub
End Namespace
Type Cursor Of T, R
    Mapper As Function(T) As R
End Type
";
    let names = extract_method_names_for_language(src, "ml");
    assert!(names.contains(&"BTreeMap_Get".to_string()), "got {names:?}");
    assert!(names.contains(&"Install".to_string()), "got {names:?}");
    assert!(
        !names.contains(&"Mapper".to_string()),
        "a field of function type must not be a method name, got {names:?}"
    );
}
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p engram_server extract_ml_method_body`
Expected: FAIL to compile — `cannot find function \`extract_ml_method_body\``. This is a
whole-crate compile error (the new tests in `business_logic_service.rs`, `detect_language_recognises_minilang`
and `extract_method_names_finds_minilang_declarations`, fail to build for the same reason), so a
single filter is enough to observe it — `cargo test` never reaches the point of applying the
filter until the crate compiles.

- [ ] **Step 3: Implement `extract_ml_method_body`**

In `crates/engram_server/src/services/full_project_migration_service.rs`, add immediately after `extract_vb_method_body`:

```rust
/// Extract a MiniLang `Function`/`Sub` body by tracking `End Function` /
/// `End Sub` depth.
///
/// MiniLang differs from VB in three ways that matter here: it has no
/// access-modifier requirement (but `Public`/`Private` are legal), generic
/// parameters sit BETWEEN the name and the parameter list
/// (`Function BTreeMap_Get Of K, V(…)`), and bodies nest `End Type` /
/// `End Unsafe` / `End Using` / `End Match` blocks that must not be counted
/// as function terminators.
pub(crate) fn extract_ml_method_body(
    content: &str,
    method_name: &str,
) -> Option<(String, u32, u32, u32)> {
    // The name may be followed by `(` or by an ` Of …` generic clause, so
    // the pattern must not demand an immediate open paren.
    let pattern = format!(
        r"(?im)^\s*(?:(?:Public|Private)\s+)?(Function|Sub)\s+{}\s*(?:\(|Of\s)",
        regex::escape(method_name)
    );
    let re = Regex::new(&pattern)
        .inspect_err(|e| tracing::warn!(method_name, error = %e, "MiniLang method body regex compile failed"))
        .ok()?;
    let m = re.find(content)?;

    let start_offset = m.start();
    let start_line = content[..start_offset].lines().count() as u32;

    let cap = re.captures(&content[m.start()..])?;
    let kind = cap[1].to_string();
    let upper_kind = kind.to_uppercase();

    // A nested declaration opener. Anchored at line start after optional
    // access modifiers, so `Mapper As Function(T) As R` never matches.
    static ML_NESTED_OPEN_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)^\s*(?:(?:Public|Private)\s+)?(?:Function|Sub)\s+\w+")
            .expect("ml_nested_open")
    });

    let after_start = &content[start_offset..];
    let match_len = m.end() - m.start();
    let decl_line_idx = after_start[..match_len.min(after_start.len())]
        .matches('\n')
        .count();

    let mut depth = 1i32;
    let mut end_pos = None;
    for (i, line) in after_start.lines().enumerate() {
        if i <= decl_line_idx {
            continue;
        }
        let trimmed = line.trim().to_uppercase();

        if !trimmed.starts_with("END ") && ML_NESTED_OPEN_RE.is_match(line.trim()) {
            depth += 1;
        }

        // Only Function/Sub terminators change depth — End Type, End If,
        // End Unsafe, End Using, End Match are unrelated nesting.
        if trimmed.starts_with("END FUNCTION") || trimmed.starts_with("END SUB") {
            depth -= 1;
            if depth == 0 && trimmed.starts_with(&format!("END {upper_kind}")) {
                let line_start = after_start
                    .lines()
                    .take(i)
                    .map(|l| l.len() + 1)
                    .sum::<usize>();
                end_pos = Some(start_offset + line_start + line.len());
                break;
            }
        }
    }

    let end_offset = end_pos.unwrap_or(content.len());
    let body = &content[start_offset..end_offset];
    let line_count = body.lines().count() as u32;
    let end_line = start_line + line_count.saturating_sub(1);

    Some((body.to_string(), start_line, end_line, line_count))
}
```

- [ ] **Step 4: Add the MiniLang language branch**

In `crates/engram_server/src/services/business_logic_service.rs`, add to `detect_language` before the `.vb` check:

```rust
    if p.ends_with(".ml") || p.ends_with(".mlinc") {
        return "ml";
    }
```

Add the method-name regex next to the existing statics:

```rust
// MiniLang declarations. Access modifiers are optional, and the name may be
// followed by an ` Of …` generic clause instead of an immediate `(` —
// demanding a paren would miss every generic declaration in the stdlib.
// Anchoring on Function/Sub as the first significant token keeps type
// annotations such as `Mapper As Function(T) As R` from matching.
static ML_METHOD_NAME_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)^\s*(?:(?:Public|Private)\s+)?(?:Function|Sub)\s+(\w+)")
        .expect("ML_METHOD_NAME_RE")
});
```

Refactor `extract_method_names` to take the detected language, and add a testable wrapper:

```rust
fn extract_method_names(content: &str) -> Vec<String> {
    let is_vb = content.contains("End Sub") || content.contains("End Function");
    extract_method_names_for_language(content, if is_vb { "vb" } else { "cs" })
}

/// Language-explicit method-name extraction. Split out from
/// `extract_method_names` so MiniLang callers (which cannot be told apart
/// from VB by content alone — both use `End Function`) can select the right
/// pattern from the file extension instead.
pub(crate) fn extract_method_names_for_language(content: &str, language: &str) -> Vec<String> {
    let re = match language {
        "ml" => &*ML_METHOD_NAME_RE,
        "vb" => &*VB_METHOD_NAME_RE,
        _ => &*CS_METHOD_NAME_RE,
    };
```

Keep the remainder of the original `extract_method_names` body (the `skip_keywords` filter and the capture loop) inside `extract_method_names_for_language`.

- [ ] **Step 5: Dispatch body extraction on the MiniLang branch**

In `analyze_file_logic` (`business_logic_service.rs` ~line 649), replace the body-extraction conditional:

```rust
        let body_opt = match language {
            "ml" => crate::services::full_project_migration_service::extract_ml_method_body(
                content, name,
            ),
            "vb" => extract_vb_method_body(content, name),
            _ => extract_cs_method_body(content, name),
        };
```

Update the `extract_method_names(content)` call in the same function to `extract_method_names_for_language(content, language)`.

Add `extract_ml_method_body` to the import list at the top of `business_logic_service.rs`.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p engram_server extract_ml_method_body detect_language_recognises_minilang extract_method_names_finds_minilang`
Expected: PASS (5 tests)

- [ ] **Step 7: Commit**

```bash
git add crates/engram_server/src/services/business_logic_service.rs crates/engram_server/src/services/full_project_migration_service.rs
git commit -m "feat(minilang): method-body extraction parity

.ml previously resolved to 'vb' by accident -- detect_language's
fallback sniffs for End Function, which MiniLang has. It worked by luck
and misread End Type/End Match. Adds an explicit ml branch, a name
regex tolerant of the Of clause between name and parameter list, and a
body extractor that treats only End Function/End Sub as terminators."
```

---

### Task 10: Gate, planning, and rendering parity

**Files:**
- Modify: `crates/engram_server/src/services/pre_commit_review_service/gates.rs` (`complexity_gate_ext` ~line 2933, `check_style_compliance` ~line 530)
- Modify: `crates/engram_server/src/handlers/planning_tools.rs` (`interface_pair_candidates` ~line 269, `is_api_code_path` ~line 321)
- Modify: `crates/engram_server/src/services/produce_claude_md_service.rs` (`language_to_globs` ~line 261, `language_display` ~line 287)
- Modify: `crates/engram_server/src/services/code_review_ingest_service.rs` (~line 1623)
- Test: inline `#[cfg(test)]` in `gates.rs` and `produce_claude_md_service.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: no new public API — behavioural parity only.

- [ ] **Step 1: Write the failing tests**

Append to the `#[cfg(test)] mod tests` block in `crates/engram_server/src/services/pre_commit_review_service/gates.rs`:

```rust
#[test]
fn minilang_files_get_the_complexity_gate() {
    // Some(true) = End-Function terminator style. Returning None would make
    // gate 16 silently skip every MiniLang file.
    assert_eq!(complexity_gate_ext("Std.Collections.List.ml"), Some(true));
    assert_eq!(complexity_gate_ext("shared.mlinc"), Some(true));
    assert_eq!(complexity_gate_ext("Form1.vb"), Some(true));
    assert_eq!(complexity_gate_ext("Program.cs"), Some(false));
    assert_eq!(complexity_gate_ext("README.md"), None);
}
```

Append to the `#[cfg(test)] mod tests` block in `crates/engram_server/src/services/produce_claude_md_service.rs`:

```rust
#[test]
fn minilang_renders_real_globs_and_display_name() {
    // The `**/*.{other}` fallback would emit the useless glob
    // `**/*.minilang`, which matches nothing.
    assert_eq!(language_to_globs("minilang"), "**/*.ml,**/*.mlinc");
    assert_eq!(language_to_globs("ml"), "**/*.ml,**/*.mlinc");
    assert_eq!(language_display("minilang"), "MiniLang");
}
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p engram_server minilang`
Expected: FAIL — `complexity_gate_ext` returns `None`; `language_to_globs` returns `"**/*.minilang"`.

- [ ] **Step 3: Fix the complexity gate**

In `crates/engram_server/src/services/pre_commit_review_service/gates.rs`, change `complexity_gate_ext`:

```rust
fn complexity_gate_ext(path: &str) -> Option<bool> {
    // Some(true) = VB-style (End Function terminators), Some(false) = brace-style.
    let l = path.to_ascii_lowercase();
    // MiniLang uses End Function/End Sub terminators like VB.
    if l.ends_with(".vb") || l.ends_with(".ml") || l.ends_with(".mlinc") {
        Some(true)
    } else if [".cs", ".ts", ".tsx", ".js", ".jsx"]
        .iter()
        .any(|e| l.ends_with(e))
```

- [ ] **Step 4: Fix style compliance**

In `check_style_compliance` (same file, ~line 530), add alongside the existing `is_vb` / `is_csharp` / `is_ts_js` flags:

```rust
    let is_minilang = {
        let l = file_path.to_ascii_lowercase();
        l.ends_with(".ml") || l.ends_with(".mlinc")
    };
```

Exactly one branch needs a MiniLang arm: the `ConventionCategory::MethodNaming` regex chain. MiniLang needs its **own** pattern rather than reusing `is_vb`'s — the VB regex demands `Sub|Function <name>(`, which misses every generic declaration, since MiniLang's `Of` clause sits between the name and the parenthesis. Add a branch after the `is_vb` one:

```rust
                } else if is_minilang {
                    // Access modifiers optional; the name may be followed by
                    // an ` Of …` generic clause instead of `(`.
                    Regex::new(r"(?im)^\s*(?:(?:Public|Private)\s+)?(?:Sub|Function)\s+(\w+)\s*(?:\(|Of\s)").ok()
```

Leave `ConventionCategory::ContextInjection` gated on `is_vb` alone — it checks for a VB `DataContext` parameter convention that has no MiniLang analogue.

- [ ] **Step 5: Fix the planning predicates**

In `crates/engram_server/src/handlers/planning_tools.rs`, extend `interface_pair_candidates`:

```rust
    let is_class_file = (ps.ends_with(".vb") || ps.ends_with(".cs") || ps.ends_with(".ml"))
```

and `is_api_code_path`:

```rust
    (ps.ends_with(".vb") || ps.ends_with(".cs") || ps.ends_with(".ts") || ps.ends_with(".js") || ps.ends_with(".ml"))
```

- [ ] **Step 6: Fix rendering and review-ingest**

In `crates/engram_server/src/services/produce_claude_md_service.rs`, add to `language_to_globs`:

```rust
        "minilang" | "ml" => "**/*.ml,**/*.mlinc".into(),
```

and to `language_display`:

```rust
        "minilang" | "ml" => "MiniLang",
```

In `crates/engram_server/src/services/code_review_ingest_service.rs` (~line 1623), add to the language-tag match:

```rust
        "ml" | "mlinc" => "minilang".into(),
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test -p engram_server minilang`
Expected: PASS

- [ ] **Step 8: Commit**

```bash
git add crates/engram_server/src/services/pre_commit_review_service/gates.rs crates/engram_server/src/handlers/planning_tools.rs crates/engram_server/src/services/produce_claude_md_service.rs crates/engram_server/src/services/code_review_ingest_service.rs
git commit -m "feat(minilang): gate, planning, and rendering parity

complexity_gate_ext returned None for .ml, so gate 16 silently skipped
every MiniLang file. produce_claude_md's fallback emitted the useless
glob **/*.minilang. Also adds .ml to interface pairing (the stdlib uses
the paired-interface-file convention) and api-path detection."
```

---

### Task 11: MiniLang diagnostics + full-suite verification

**Files:**
- Create: `crates/engram_index/src/language_diagnostics/minilang.rs`
- Modify: `crates/engram_index/src/language_diagnostics/mod.rs`
- Modify: `crates/engram_server/src/handlers/access_layer_tools.rs` (~line 3152)
- Test: inline `#[cfg(test)]` in `minilang.rs`

**Interfaces:**
- Consumes: `LanguageDiagnostic` from `super`.
- Produces: `LanguageFamily::MiniLang`; `pub fn detect(code_files: &[(&str, &str)]) -> Vec<LanguageDiagnostic>`.

- [ ] **Step 1: Write the failing test**

Create `crates/engram_index/src/language_diagnostics/minilang.rs` with only the test block for now:

```rust
#[cfg(test)]
mod tests {
    use super::detect;

    #[test]
    fn flags_the_documented_minilang_footguns() {
        let src = "\
Function Logger() As Int
    While True
        Say 1
    End While
    Return 0
End Function
Function Boot() As Int
    Spawn Call Logger()
    Unsafe
        Say 2
    End Unsafe
    Var p As Int
    Set p To Std.Memory.Alloc(64)
    Return 0
End Function
";
        let files = vec![("Boot.ml", src)];
        let out = detect(&files);
        let cats: Vec<&str> = out.iter().map(|d| d.category.as_str()).collect();

        assert!(cats.contains(&"non_detached_spawn"), "got {cats:?}");
        assert!(cats.contains(&"bare_unsafe"), "got {cats:?}");
        assert!(cats.contains(&"unfreed_alloc"), "got {cats:?}");
    }

    #[test]
    fn clean_source_produces_no_findings() {
        let src = "\
Function Boot() As Int
    Spawn Detached Call Logger()
    Unsafe(RawPtr)
        Say 2
    End Unsafe
    Return 0
End Function
";
        let files = vec![("Clean.ml", src)];
        assert!(detect(&files).is_empty(), "clean source must not fire");
    }
}
```

- [ ] **Step 2: Run it to make sure it fails**

Run: `cargo test -p engram_index minilang`
Expected: FAIL to compile — `cannot find function \`detect\``

- [ ] **Step 3: Implement the diagnostics**

Prepend to `crates/engram_index/src/language_diagnostics/minilang.rs`:

```rust
//! MiniLang pre-edit risk diagnostics. Flags the footguns the language
//! reference documents as hazards, so an agent editing a `.ml` method gets
//! the same "what to watch out for" signal the VB/C#/C/C++/Rust modules
//! provide.

use regex::Regex;
use std::sync::LazyLock;

use super::LanguageDiagnostic;

/// The word following `Spawn`. The `regex` crate has no look-around, so
/// the "not Detached" condition is checked on the capture rather than in
/// the pattern. The root scope joins every non-detached child before exit,
/// so spawning a non-terminating fiber hangs the program.
static SPAWN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*Spawn\s+(\w+)").expect("ml spawn"));
/// Bare `Unsafe` grants every capability the compiler can reason about.
/// A capability-granular block is nearly always the right call.
static BARE_UNSAFE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*Unsafe\s*$").expect("ml bare unsafe"));
static ALLOC_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bStd\.Memory\.Alloc\s*\(").expect("ml alloc"));

pub fn detect(code_files: &[(&str, &str)]) -> Vec<LanguageDiagnostic> {
    let mut out = Vec::new();
    for (file, content) in code_files {
        let has_free = content.contains("Std.Memory.Free");
        let has_arena = content.contains("Using Arena");

        for (idx, line) in content.lines().enumerate() {
            let line_no = idx + 1;
            let trimmed = line.trim();
            // MiniLang accepts ', # and // as comment markers.
            if trimmed.starts_with('\'') || trimmed.starts_with('#') || trimmed.starts_with("//") {
                continue;
            }

            if let Some(c) = SPAWN_RE.captures(line) {
                if &c[1] != "Detached" {
                    out.push(LanguageDiagnostic {
                        location: format!("{file}:{line_no}"),
                        category: "non_detached_spawn".to_string(),
                        severity: "medium".to_string(),
                        evidence: trimmed.to_string(),
                        guidance: "The root scope joins this fiber before the program exits. If \
                                   it never terminates the program hangs — use `Spawn Detached` \
                                   for daemons, loggers, and watchdogs."
                            .to_string(),
                    });
                }
            }

            if BARE_UNSAFE_RE.is_match(line) {
                out.push(LanguageDiagnostic {
                    location: format!("{file}:{line_no}"),
                    category: "bare_unsafe".to_string(),
                    severity: "medium".to_string(),
                    evidence: trimmed.to_string(),
                    guidance: "Bare `Unsafe` grants every capability `All` covers. Narrow it to \
                               what the block actually needs — `Unsafe(RawPtr)`, \
                               `Unsafe(Alloc)`, `Unsafe(Asm)` — so a capability added in a later \
                               release does not silently widen this block."
                        .to_string(),
                });
            }

            if ALLOC_RE.is_match(line) && !has_free && !has_arena {
                out.push(LanguageDiagnostic {
                    location: format!("{file}:{line_no}"),
                    category: "unfreed_alloc".to_string(),
                    severity: "high".to_string(),
                    evidence: trimmed.to_string(),
                    guidance: "This file allocates with `Std.Memory.Alloc` but never calls \
                               `Std.Memory.Free` and opens no `Using Arena` scope. Pair the \
                               allocation with a free, or bump-allocate inside an arena."
                        .to_string(),
                });
            }
        }
    }
    out
}
```

- [ ] **Step 4: Register the family**

In `crates/engram_index/src/language_diagnostics/mod.rs`, add the module declaration:

```rust
pub mod minilang;
```

Add the enum variant:

```rust
    /// MiniLang — native systems language with capability-granular unsafe.
    MiniLang,
```

Add the dispatch arm in `detect_language_diagnostics`:

```rust
        LanguageFamily::MiniLang => {
            let diagnostics = minilang::detect(code_files);
            LanguageDiagnosticReport::new("minilang", diagnostics, code_files.len())
        }
```

In `crates/engram_server/src/handlers/access_layer_tools.rs` (~line 3161), add to the extension-to-family match:

```rust
                    "ml" | "mlinc" => {
                        Some(engram_index::language_diagnostics::LanguageFamily::MiniLang)
                    }
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p engram_index minilang`
Expected: PASS (2 tests)

- [ ] **Step 6: Run the full suite**

Run: `cargo fmt --all && cargo check --all-targets && cargo test -p engram_index -p engram_server --tests --lib --no-fail-fast`
Expected: green. Use the Bash tool, not PowerShell with `2>&1` — benign stderr WARNs become `NativeCommandError` and report exit 255 on a green suite.

- [ ] **Step 7: Verify against the real corpus**

The unit tests prove the grammar on synthetic snippets; this proves it on 5,279 real files.

Stop the daemon first — deploying over a running binary disconnects the session's Engram MCP client:

```bash
cargo build --release
```

Deploy and index, driving the server headlessly:

```bash
cp target/release/engram_server.exe "$LOCALAPPDATA/engram/bin/engram_server.exe"
python target/engram_drive.py tool index_project \
  '{"directory":"C:/Users/Dennis/source/repos/MiniLangCompiler","project_type":"minilang","wait":true}'
```

Then confirm the graph actually populated:

```bash
python target/engram_drive.py inspect <project_id>
```

Acceptance: the language histogram reports `minilang` at roughly 5,279 files (not 0, and not merely the 781 `.vb`), and the node-type counts include non-zero `struct`, `union`, `extern_function`, and `ui_container`. A zero in any of those means a construct family is not reaching the graph even though its unit test passes — most likely the `hybrid.rs` dispatch arm from Task 8 is unreachable because an earlier branch claimed the extension.

Record the observed counts in the commit message. If `.lock` files accumulated from a force-killed daemon, clear them before restarting (only with zero `engram_server` processes running):

```bash
find "$LOCALAPPDATA/engram/data/projects" -name '*.lock' -delete
```

- [ ] **Step 8: Commit**

```bash
git add crates/engram_index/src/language_diagnostics/ crates/engram_server/src/handlers/access_layer_tools.rs
git commit -m "feat(minilang): pre-edit risk diagnostics

Adds LanguageFamily::MiniLang with heuristics for the documented
footguns: non-Detached Spawn of a possibly non-terminating fiber, bare
Unsafe granting more capability than needed, and Alloc with no matching
Free outside an arena scope. Wires .ml/.mlinc into
get_method_edit_context's diagnostics dispatch."
```

---

## Notes for the implementer

- **`LANGUAGE.md` is not authoritative.** It omits `Sub`, access modifiers, `Throws`, and generic constraints. When a construct's shape is unclear, grep the corpus at `C:\Users\Dennis\source\repos\MiniLangCompiler\src` — that is the ground truth.
- **The two easiest mistakes**, both guarded by tests: matching `Function\s+(\w+)\s*\(` (misses generic declarations, since the `Of` clause intervenes), and treating `As Function(T) As R` field rows as declarations.
- **Do not add `.expected`/`.error` to `default_exts`.** Those names are generic enough to pull junk into unrelated repositories; they belong to the MiniLang preset only.
- If a test's expected count in `ml_corpus_smoke_test.rs` disagrees with the implementation, determine which is right by checking the corpus before changing either. A shrinking count is the signature of a silently mis-parsed construct family.
