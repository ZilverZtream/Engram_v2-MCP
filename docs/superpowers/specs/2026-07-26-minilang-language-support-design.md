# MiniLang (`.ml`) Language Support in Engram

**Date:** 2026-07-26
**Status:** Design — approved for planning
**Bar:** MiniLang support must be *at least as good* as the existing VB.NET support, measured
across every language-dispatched site in the codebase, not just the extractor.

---

## Problem

Engram cannot see MiniLang source. Pointed at `MiniLangCompiler`, it indexes the 781 `.vb`
compiler files and ignores the 5,279 `.ml` + 5 `.mlinc` files — roughly 87% of the repository's
source, including the entire standard library, every conformance test, and every example.

The failure is at the first gate, not the extractor: `.ml` appears in no extension preset
(`engram_server/src/utils/files.rs`), so the ingest walker never yields those paths. Nothing
downstream ever runs. Three further layers would fail even if the walker did yield them:
`guess_language` maps `.ml` to `"text"`, `hybrid.rs` has no dispatch arm, and no extractor exists.

## Language summary

MiniLang is a native systems language for Windows x64 with VB-flavoured, line-oriented,
block-structured syntax. Blocks open with a keyword and close with `End <keyword>`
(`End Function`, `End Type`, `End Namespace`, `End Enum`, `End Interface`, `End Match`,
`End Unsafe`, `End Using`, `End Asm`, `End Ui`, `End Try`). There is no tree-sitter grammar,
so extraction is hand-rolled and line-based — the same approach as `asp_classic_extractor.rs`.

> **`LANGUAGE.md` is not a sufficient grammar reference.** It documents neither `Sub`, access
> modifiers, `Throws` clauses, nor generic constraints — all of which are widespread in the
> shipped standard library. The grammar below was derived from the actual corpus
> (`src/`, `examples/`, `benchmarks/`) and the corpus is the authority. Counts are from that survey.

The real declaration grammar:

```
[Public|Private] Function Name [Of T[ As Constraint][, U…]](params) [As RetType[?]] [Throws ErrType]
[Public|Private] Sub      Name [Of T[ As Constraint][, U…]](params)                 [Throws ErrType]
[Public]         Type     Name [Of T[, U…]] [Implements Interface]
                 Interface Name
                 Enum      Name
```

Parameters are `[Borrow|BorrowMut] name As Type`.

Three properties of this grammar drive extractor correctness:

1. **The `Of` clause sits between the name and the parameter list** — `Function BTreeMap_Get Of
   K, V(Borrow tree As …)`. A `Function\s+(\w+)\s*\(` pattern misses every generic declaration
   (400+ in the stdlib). The name must be matched independently of what follows it.
2. **`As Function(…)` is a type annotation, not a declaration.** Parameter and field rows like
   `Mapper As Function(T) As R` and `BorrowMut handler As Function(Int) As Int` must never
   register as function declarations. Declaration matching anchors on `Function`/`Sub` as the
   first token after optional access modifiers, not on the keyword appearing anywhere.
3. **`'` is unambiguously a comment marker.** MiniLang has no character-literal syntax — `Char`
   values are produced by `Std.Convert.IntToChar`. Comment stripping only needs to respect
   double-quoted strings and their `\` escapes.

Constructs that carry indexable meaning:

- `Namespace A.B` … `End Namespace` — nests; establishes FQNs (259 uses)
- `Function F(…) As R` … `End Function` (3,585) and `Sub S(…)` … `End Sub` (686, void procedure)
  — optional `Public`/`Private`; `Of T [As Constraint]` generics; `?`-suffixed nullable returns;
  `Borrow`/`BorrowMut` parameter modes; `Throws ErrType` clause (1,000+ uses)
- `Type T` … `End Type` — a **struct** when rows are `Name As Type`, a **discriminated union**
  when rows are `Variant(payload…)` or bare `Variant`; `Implements I` clause; `Of T, E` generics
- `Enum E` … `End Enum` — optional explicit values
- `Interface I` … `End Interface`
- `Const NAME = expr` — CTFE-evaluated
- `Include "path.ml"` — textual include, resolved relative to the including file
- `Declare Function F Lib "…" [Alias "…"]` (P/Invoke) and
  `Extern "C" [Blocking] Function F Lib "…"` (C-FFI), both inside `Unsafe(Ffi)`
- `Unsafe(RawPtr, Alloc, Asm, Ffi, All)` — capability-granular unsafe blocks
- `Spawn [Detached] [Hi|Lo|Normal] Call f(…)`, `Channel(Of T)`, `Send`/`Receive`/`Close`
- `Vector128/256/512(Of T)` and the `Std.Vector.*` intrinsic family
- `Ui … End Ui` — declarative UI DSL. Container elements observed in the corpus:
  `Panel`, `Label`, `Button`, `Badge`, `Card`, `Field`, `Checkbox`, `Radio`, `Switch`, `Slider`,
  `ProgressBar`, `Image`, `Divider`, `VStack`; attribute rows: `Rect`, `Bg`, `Text`, `Style`,
  `Border`, `Gradient`, `Shadow`
- `Asm … End Asm` — inline x64 assembly with `In`/`Out` bindings
- `Select`/`Switch`/`SelectChannel` … `End <kw>` — additional block terminators the scanner
  must balance even though they emit no symbols
- `Using Arena a` … `End Using`, `Try`/`Catch`/`Finally`, `Ref(Of T)`/`Weak(Of T)`

Comments are `'`, `#`, or `//`. There is **no doc-comment convention** (no `'''` equivalent).

Conformance tests pair a source file with golden sidecars: `foo.ml` ↔ `foo.expected`
(stdout oracle) and `foo.error` (expected `MLC####` diagnostics), plus `foo.exitcode`.

---

## Design

### 1. Ingestion — make `.ml` visible

| Site | Change |
|---|---|
| `engram_core/src/types.rs` `guess_language` | `"ml" \| "mlinc"` → `"minilang"` |
| `engram_index/src/parsing.rs` `ext_to_static` | `"ml" => "ml"`, `"mlinc" => "mlinc"` |
| `engram_server/src/utils/files.rs` `default_exts` | add `"ml"`, `"mlinc"` |
| `engram_server/src/models/requests.rs` `ProjectType` | new `MiniLang` variant |
| `engram_server/src/utils/files.rs` | new `minilang_exts()` preset |

`ProjectType::MiniLang` (serde aliases `minilang`, `mini_lang`, `ml`; accepted by
`from_registry_str`) indexes the polyglot reality of a MiniLang compiler repo — MiniLang
stdlib and tests, a VB.NET compiler, and C/Rust/Go external-ABI fixtures:

```
ml, mlinc, expected, error, exitcode, vb, vbproj, sln, cs, csproj,
c, rs, go, ps1, sh, md, json, yaml, yml, txt, snapshot
```

Golden extensions (`expected`, `error`, `exitcode`) are deliberately **excluded** from
`default_exts` — those names are too generic to pull into unrelated repositories.

`.ml` is always treated as MiniLang. This is a deliberate, accepted trade: an OCaml
repository indexed by Engram would have its `.ml` files parsed as MiniLang. Such files stay
searchable; they simply yield few or no symbols. Recorded here so the decision is not
rediscovered as a bug.

### 2. Extractor — `crates/engram_index/src/ml_extractor/`

A directory module, split by concern rather than one oversized file:

- `mod.rs` — `extract_ml()` entry point, block scanner, namespace/FQN stack, dispatch
- `decls.rs` — Namespace, Function, Type, Enum, Interface, Const, Include, Declare, Extern
- `bodies.rs` — call sites, capabilities, concurrency, SIMD, ownership metadata
- `ui.rs` — the declarative `Ui` DSL

**Symbols.** A symbol's `kind` becomes its graph node type directly, via the generic branch in
`ingest_service.rs`. No node-type registry change is required.

| Construct | kind | metadata |
|---|---|---|
| `Namespace A.B` | `namespace` | nests; prefixes every FQN inside |
| `Function F(…) As R` / `Sub S(…)` | `function` | `is_sub` flag, access modifier, params with binding mode, return type, nullable flag, generic params with constraints, `throws` type; a first parameter named `this` also emits `Contains` Type→Function (MiniLang's method convention) |
| `Type T` with `X As Int` rows | `struct` | fields; `Ref(Of T)` vs `Weak(Of T)` marked strong/weak |
| `Type T` with variant rows | `union` | variant names and payload arity |
| `Enum E` | `enum` | members with explicit values |
| `Interface I` | `interface` | member signatures |
| `Const N = expr` | `constant` | CTFE expression text |
| `Declare Function … Lib` | `extern_function` | `binding=pinvoke`, library, alias |
| `Extern "C" … Function … Lib` | `extern_function` | `binding=c_ffi`, blocking flag |
| `Ui` / `Panel` / `Label` / `Badge` | `ui_container`, `control` | `Rect` geometry, colors, text |
| `Asm … End Asm` | `inline_asm` | mnemonics, `In`/`Out` signature |

**Edges.**

| Edge | raw kind | shape |
|---|---|---|
| calls — plain, `Call f(x)`, `Spawn [Detached] Call f(x)`, qualified `Std.X.Y Of Int(…)` | `calls` | enclosing function → callee FQN; metadata flags spawn/detached/priority/generic args |
| `Include "x.ml"` | `includes_file` | file → file, resolved relative to the includer |
| `Type T Implements I` | `implements_interface` | struct/union → interface |
| `Function F(…) Throws E` | `dependency` | function → error type, metadata `relation=throws`. 1,000+ in the stdlib; makes "what can this fail with" answerable |
| `Unsafe(RawPtr, Ffi)` | `dependency` | function → capability, metadata lists granted capabilities |
| `Send`/`Receive`/`Close`/`NewChannel(Of T)` | `calls` | plus `concurrency=channel`, element type |
| `Std.Vector.*128/256/512 Of T` | `calls` | plus `simd_width`, `lane_type` |
| `foo.ml` ↔ `foo.expected` / `foo.error` | `test_oracle` | source file → golden file |

**Top-level statements.** A MiniLang file is script-style: statements outside any `Function`
form the program entry point (`Say fib(15)`, a bare `Ui` block, `Spawn Call Worker(42)`). Each
file containing such statements emits one `function` symbol named `<module>` spanning the
first through last top-level statement line, with metadata `synthetic=module_entry`. Calls made
from top-level statements attribute to it, so a conformance test's single `Say Foo(…)` line
still produces a real caller in the graph rather than a dangling edge. Files with no top-level
statements (the stdlib modules, which are pure declarations) emit no such symbol.

The oracle edge stats the sibling on disk before emitting. Emitting unconditionally from the
naming convention would mint phantom targets for the thousands of `.ml` files that have no
golden — the same class of defect previously seen with triple-slash reference targets.

`extract_ml` takes both the absolute path (to stat siblings) and the project-relative path
(to build edge targets), mirroring the WinForms-designer branch. Edge targets must be
project-relative; absolute targets are rejected by the ingest safety check.

### 3. New `EdgeKind::TestOracle`

Five touch points: the enum, `ALL`, `as_str`, and `parse` in `engram_graph/src/store.rs`, plus
the raw-kind map in `ingest_service.rs`. Wire string: `"test_oracle"`. Without a distinct kind
the pairing collapses into the `Dependency` catch-all and cannot be queried separately.

### 4. Ownership and safety metadata

Attached to symbols and edges so `check_edit_safety` and `get_method_edit_context` can warn
before an edit rather than after a failed compile:

- parameter binding mode — owned, `Borrow`, `BorrowMut`
- local mutability — `Dim` (immutable) vs `Var`/`Mut` (mutable)
- strong `Ref(Of T)` vs `Weak(Of T)` struct fields
- `Using Arena` scopes, and `Try`/`Catch`/`Finally`/`Throw` regions

### 5. Parity with the VB tool surface

An audit of every VB-dispatched site. Each row is either a required MiniLang branch or an
explicit no-analogue finding.

**Requires a MiniLang branch:**

| Site | Why |
|---|---|
| `business_logic_service.rs` `detect_language` | `.ml` currently resolves to `"vb"` *by accident* — the fallback sniffs for `End Function`, which MiniLang has. It works by luck and misreads `End Type`/`End Match`/`End Ui`. Needs an explicit `"ml"` branch. |
| `full_project_migration_service.rs` | New `extract_ml_method_body`: `End Function`/`End Sub` depth tracking with optional `Public`/`Private`, tolerant of an `Of T, U` clause before the parameter list, and of nested `End Type`/`End Unsafe`/`End Using`/`End Match` blocks |
| `business_logic_service.rs` `extract_method_names` | New `ML_METHOD_NAME_RE` matching `Function`/`Sub` with optional access modifier, name captured independently of a following `Of` clause. Must **not** match first-class-function type annotations such as `Mapper As Function(T) As R` |
| `gates.rs` `complexity_gate_ext` | `.ml`/`.mlinc` → `Some(true)` (End-Function terminator style). Without this, gate 16 (complexity/params) silently skips every MiniLang file |
| `gates.rs` `check_style_compliance` | MiniLang style flag alongside `is_vb` / `is_csharp` / `is_ts_js` |
| `planning_tools.rs` `interface_pair_candidates` | Add `.ml` — `Std.IO.IStream.ml` is a real interface file following the paired-file convention |
| `planning_tools.rs` `is_api_code_path` | Add `.ml` |
| `produce_claude_md_service.rs` | `language_to_globs` → `**/*.ml,**/*.mlinc`; `language_display` → `MiniLang`. The `**/*.{other}` fallback would otherwise emit the useless glob `**/*.minilang` |
| `code_review_ingest_service.rs` | Language tag `minilang` |
| `language_diagnostics/` | New `minilang.rs` + `LanguageFamily::MiniLang`, matching the existing `vb.rs` |

**MiniLang diagnostics** (`language_diagnostics/minilang.rs`) covers documented footguns:
non-`Detached` `Spawn` of a non-terminating fiber (hangs the root scope at exit); definite
strong-`Ref` self-cycle (MLC6013); bare `Unsafe` granting more capability than the block uses;
`Send` on a closed channel; `Match` without `Case Else` over an open variant set; raw
`Std.Memory.Alloc` with no matching `Free` outside an arena scope.

**No analogue required** — recorded so these are not later mistaken for gaps:

| Site | Reason |
|---|---|
| Gate 17 (XML docs) | MiniLang has no doc-comment convention |
| `guard_parity` gate | Web-endpoint specific |
| SQL, controls, settings, ViewState, Crystal Reports, WebForms extractors | No MiniLang analogue |
| `vb_translation_traps` | VB→C# migration specific |

**Already correct, no change:**

| Site | Reason |
|---|---|
| `chunking.rs` `semantic_chunk_lines` | Symbol-driven and language-agnostic — MiniLang functions chunk on their own boundaries once the extractor emits line ranges |
| `parsing.rs` `is_comment_line` | Already handles `'`, `#`, and `//` |
| `pre_commit_review_service.rs` `is_test_path` | The `/tests/` path rule already covers `tests/conformance/…` |

### 6. Dispatch

`hybrid.rs` gains an `ext_lower == "ml" | "mlinc"` arm ahead of the generic tree-sitter
fallback, passing both the absolute and project-relative paths.

---

## Testing

- **Unit tests per construct** in `ml_extractor` — small synthetic snippets covering each row
  of the symbol and edge tables, including the negative cases: a `Function` type annotation in
  a field row must not register as a declaration; an `Include` of a missing file must not mint
  a phantom node; a `.ml` file with no golden must emit no `test_oracle` edge.
- **Body-extraction tests** for `extract_ml_method_body`: nested functions, a function
  containing a `Type` block, a function containing `Unsafe`/`Using`/`Match` blocks.
- **Corpus smoke test** over a checked-in sample of representative MiniLang shapes, asserting
  symbol and edge counts do not regress.
- **Preset test** asserting `ProjectType::MiniLang` round-trips through `from_registry_str`
  and yields the documented extension list.

## Out of scope

**Compiler-assisted extraction.** `MiniLangCompiler.LanguageServer` and the compiler service
API could supply exact ASTs the way the Roslyn sidecar does for VB. The language server is
currently too basic to build on, and a sidecar couples Engram to a compiler binary. Line-based
extraction now; revisit if accuracy demands it.
