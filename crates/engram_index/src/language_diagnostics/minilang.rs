//! MiniLang pre-edit risk diagnostics. Flags the footguns the language
//! reference documents as hazards, so an agent editing a `.ml` method gets
//! the same "what to watch out for" signal the VB/C#/C/C++/Rust modules
//! provide.

use regex::Regex;
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;

use crate::ml_extractor::{self, decls};
use crate::parsing::ExtractedSymbol;

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

/// A `Close(ch)` call — single plain identifier argument. A dotted or
/// indexed argument (`Close(h.inbox)`, `Close(chans[1])`) is deliberately
/// NOT matched: this diagnostic has no alias/field tracking, so it only
/// ever reasons about a simple local channel binding, and silently missing
/// a dotted/indexed channel is a safe miss, not a false positive.
static CLOSE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bClose\s*\(\s*([A-Za-z_]\w*)\s*\)").expect("ml close"));
/// A `Send(ch, value)` call — same plain-identifier-only restriction as
/// `CLOSE_RE`.
static SEND_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bSend\s*\(\s*([A-Za-z_]\w*)\s*,").expect("ml send"));

/// Local-variable type bindings usable to resolve a `Match` scrutinee's
/// type: `Dim/Var/Mut IDENT As TYPE` (locals) and `Borrow/BorrowMut IDENT As
/// TYPE` (parameters written with an explicit binding-mode keyword — a
/// plain OWNED parameter has no such keyword and is handled separately by
/// `params_with_types`, since `ml_extractor::decls::parse_params` strips
/// type text that this diagnostic specifically needs back).
static LOCAL_TYPE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\b(?:Dim|Var|Mut|Borrow|BorrowMut)\s+([A-Za-z_]\w*)\s+As\s+([A-Za-z_][A-Za-z0-9_.]*)",
    )
    .expect("ml local/param type")
});

/// Control-flow constructs that can make two textually-sequential
/// statements NOT actually execute in that order (a branch offers mutually
/// exclusive arms; a loop can run zero, one, or many times, which is
/// loop-carried reasoning this diagnostic explicitly does not attempt).
/// `Try` is deliberately excluded: its body executes top-to-bottom exactly
/// like straight-line code unless an EARLIER statement in the same body
/// throws, and the real corpus's own canonical trap fixture
/// (`test_mlh2360_send_closed_caught.ml`) puts the `Send` inside a `Try`
/// specifically to catch the fault it deliberately causes — excluding
/// `Try` here is what lets that real, intentional pattern get flagged.
const BRANCH_OR_LOOP: &[&str] = &[
    "If",
    "While",
    "For",
    "Repeat",
    "Match",
    "Select",
    "SelectChannel",
];

pub fn detect(code_files: &[(&str, &str)]) -> Vec<LanguageDiagnostic> {
    let mut out = Vec::new();
    for (file, content) in code_files {
        // Shadow with an explicit `&str` type: match ergonomics binds
        // `file`/`content` as `&&str` from the `&[(&str, &str)]` slice
        // pattern, and every helper below (and `Path::new`) is written
        // against a plain `&str`.
        let file: &str = file;
        let content: &str = content;

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

        // The remaining three diagnostics need type/function-level
        // structure (struct field graphs, union variant sets, function
        // line ranges) that a single-line regex pass cannot recover.
        // Reusing `ml_extractor::extract_ml` gets that structure for free,
        // from the same battle-tested parser that already handles every
        // MiniLang one-liner/self-closing/`Try`-duality edge case — far
        // safer than re-deriving a second, independent block scanner here.
        // `abs_path` is only used by the extractor to stat conformance-test
        // golden siblings (`.expected`/`.error`/`.exitcode`); passing the
        // (possibly fabricated) project-relative path as a stand-in is
        // harmless here since we discard the edges and never look at
        // `test_oracle` output.
        let (symbols, _edges) = ml_extractor::extract_ml(Path::new(file), file, content);
        out.extend(strong_ref_cycle_diagnostics(file, &symbols));
        out.extend(send_after_close_diagnostics(file, content, &symbols));
        out.extend(match_missing_case_else_diagnostics(file, content, &symbols));
    }
    out
}

/// The last dot-separated segment of a (possibly namespace-qualified) name.
fn bare_name(name: &str) -> String {
    name.rsplit('.').next().unwrap_or(name).to_string()
}

// ---------------------------------------------------------------------
// MLC6013 — definite strong-`Ref` self-ownership cycle.
// ---------------------------------------------------------------------

/// One struct's strong-`Ref` edges, resolved within its own file only (see
/// `strong_ref_cycle_diagnostics`'s doc comment for why cross-file
/// resolution is deliberately out of scope).
struct StructNode {
    line: u32,
    /// `(field_name, target_bare_name)` — one entry per strong,
    /// non-nullable `Ref(Of Target)` field whose payload is itself a
    /// non-nullable struct reference.
    edges: Vec<(String, String)>,
}

/// One `fields` metadata entry (`name:Type`, or `name:Type:strong` /
/// `name:Type:weak` for a `Ref(Of …)`/`Weak(Of …)` field — see
/// `ml_extractor::decls::open_declaration`'s `Type` arm) into
/// `(field_name, field_type_text, is_strong_ref)`.
fn parse_field_entry(entry: &str) -> Option<(&str, &str, bool)> {
    let (name, rest) = entry.split_once(':')?;
    if let Some(ty) = rest.strip_suffix(":strong") {
        Some((name, ty, true))
    } else if let Some(ty) = rest.strip_suffix(":weak") {
        Some((name, ty, false))
    } else {
        Some((name, rest, false))
    }
}

/// The referenced type name and its own nullability inside a `Ref(Of X)`
/// field type text, e.g. `Ref(Of Node)` -> `("Node", false)`, `Ref(Of
/// Node?)` -> `("Node", true)`. `None` when `ty` is not a `Ref(Of …)` shape
/// at all (a `Weak(Of …)` field never reaches this — see caller — and a
/// nullable *field* itself, `Ref(Of Node)?`, is stripped by the caller
/// before this runs).
fn ref_pointee(ty: &str) -> Option<(String, bool)> {
    let inner = ty.strip_prefix("Ref(Of ")?;
    let mut depth = 1i32;
    let mut end = None;
    for (i, b) in inner.bytes().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let inner_ty = inner[..end?].trim();
    let (inner_ty, nullable) = match inner_ty.strip_suffix('?') {
        Some(t) => (t.trim(), true),
        None => (inner_ty, false),
    };
    let name: String = inner_ty
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '.')
        .collect();
    if name.is_empty() {
        None
    } else {
        Some((name, nullable))
    }
}

/// Does following strong edges from `start` ever reach `target`? (A path of
/// length >= 1 — a direct self-loop counts, since the caller always checks
/// an OUTGOING edge's target against the owner it came from.)
fn can_reach(start: &str, target: &str, graph: &BTreeMap<String, StructNode>) -> bool {
    let mut stack = vec![start.to_string()];
    let mut seen: HashSet<String> = HashSet::new();
    while let Some(node) = stack.pop() {
        let Some(info) = graph.get(node.as_str()) else {
            continue;
        };
        for (_field, tgt) in &info.edges {
            if tgt == target {
                return true;
            }
            if seen.insert(tgt.clone()) {
                stack.push(tgt.clone());
            }
        }
    }
    false
}

/// MLC6013: a `Type` whose strong-`Ref` fields form a cycle — directly
/// (`A.field: Ref(Of A)`) or transitively (`A.field: Ref(Of B)`, `B.field2:
/// Ref(Of A)`) — retains every member of the cycle forever, since nothing
/// ever drops the last strong reference. `Weak(Of …)` and nullable edges
/// are the documented breaks, matching the real compiler's own
/// `WarnDefiniteStrongSelfCycle` (`SemanticAnalyzer.Types.vb`), which
/// checks only the direct A->A case; this generalizes it to the
/// transitive case the compiler does not check today.
///
/// Resolution is scoped to a SINGLE FILE, not merged across
/// `code_files`. Real MiniLang conformance-test files are self-contained
/// and freely reuse short type names (`Node`, `Expr`, `Shape`, …) across
/// unrelated files with no shared namespace — a corpus-wide census run
/// while building this diagnostic found exactly this collision (a
/// same-named-but-unrelated `Expr` in a different file was picked up as
/// the referent) before this scoping was added. Restricting resolution to
/// one file trades away a hypothetical cross-file cycle (never observed in
/// the real corpus — both real cycle fixtures are single-file) for
/// eliminating that false-positive class entirely. A bare name that
/// collides more than once WITHIN a single file is treated as ambiguous
/// and dropped, rather than guessing which declaration a field refers to.
fn strong_ref_cycle_diagnostics(
    file: &str,
    symbols: &[ExtractedSymbol],
) -> Vec<LanguageDiagnostic> {
    let mut nodes: BTreeMap<String, Option<StructNode>> = BTreeMap::new();

    for sym in symbols {
        if sym.kind != "struct" {
            continue;
        }
        let bare = bare_name(&sym.name);
        let fields_str = sym
            .metadata
            .as_ref()
            .and_then(|m| m.get("fields"))
            .map(String::as_str)
            .unwrap_or("");

        let mut edges = Vec::new();
        for entry in fields_str.split("||").filter(|s| !s.is_empty()) {
            let Some((fname, ty, is_strong)) = parse_field_entry(entry) else {
                continue;
            };
            if !is_strong {
                continue;
            }
            let (ty_clean, field_nullable) = match ty.strip_suffix('?') {
                Some(t) => (t, true),
                None => (ty, false),
            };
            // A nullable strong-Ref field (`Peer As Ref(Of Node)?`) can be
            // absent, so it is not a DEFINITE cycle edge — matches the real
            // compiler's `field.IsNullable` check.
            if field_nullable {
                continue;
            }
            let Some((target, target_nullable)) = ref_pointee(ty_clean) else {
                continue;
            };
            // Matches the compiler's `pointee.IsNullable` check: `Ref(Of
            // Node?)` is not observed in the real corpus, but the same
            // "absence can terminate the chain" reasoning applies.
            if target_nullable {
                continue;
            }
            edges.push((fname.to_string(), bare_name(&target)));
        }

        let node = StructNode {
            line: sym.start_line,
            edges,
        };
        match nodes.entry(bare) {
            Entry::Occupied(mut e) => {
                e.insert(None); // duplicate bare name in this file: ambiguous
            }
            Entry::Vacant(e) => {
                e.insert(Some(node));
            }
        }
    }

    let graph: BTreeMap<String, StructNode> = nodes
        .into_iter()
        .filter_map(|(k, v)| v.map(|n| (k, n)))
        .collect();

    let mut out = Vec::new();
    for (name, info) in &graph {
        let culprit = info
            .edges
            .iter()
            .find(|(_field, target)| target == name || can_reach(target, name, &graph));
        let Some((field, target)) = culprit else {
            continue;
        };
        out.push(LanguageDiagnostic {
            location: format!("{file}:{}", info.line),
            category: "strong_ref_self_cycle".to_string(),
            severity: "medium".to_string(),
            evidence: format!("Type {name}: {field} As Ref(Of {target})"),
            guidance: format!(
                "MLC6013: `{name}.{field}` is a strong, non-nullable `Ref(Of {target})` that \
                 closes an ownership cycle back to `{name}` (directly, or through other strong \
                 Ref fields). Every member of the cycle retains the others forever — the \
                 allocation can never reach an acyclic terminal edge and leaks. Change this \
                 field (or one of the intermediate strong-Ref fields in the cycle) to \
                 `Weak(Of {target})`, or make it nullable, to give the cycle a break."
            ),
        });
    }
    out
}

// ---------------------------------------------------------------------
// Send on a channel already Close()d in the same function body.
// ---------------------------------------------------------------------

/// After `Close(ch)`, a `Send(ch, v)` raises a runtime fault (exit code
/// -5, per `tests/conformance/channels-typed/test_send_on_closed_channel_traps.ml`).
/// Flags a `Send` whose channel was unambiguously `Close`d earlier in the
/// SAME function body, on a textually identical simple identifier — no
/// alias/field tracking, no cross-function reasoning, and no loop-carried
/// reasoning (a `Close`/`Send` pair is only trusted when NEITHER sits
/// inside an `If`/`Match`/`Select`/`SelectChannel`/`While`/`For`/`Repeat`
/// nesting level, i.e. `branch_depth == 0` for both — see `BRANCH_OR_LOOP`).
/// `Try` is deliberately transparent (does not count toward branch depth):
/// see `BRANCH_OR_LOOP`'s doc comment for why, validated against the real
/// corpus's own canonical fixture for this exact fault. Transparency stops
/// at `Catch`/`Finally`, which are only reached when an earlier statement
/// in the `Try` body threw — any `Close` recorded inside that body may not
/// have run, so `closed` rolls back to the Try's entry snapshot there.
/// Scanning is scope-isolated per narrowest enclosing function, so the
/// synthetic `<module>` entry never judges a nested function's channels.
fn send_after_close_diagnostics(
    file: &str,
    source: &str,
    symbols: &[ExtractedSymbol],
) -> Vec<LanguageDiagnostic> {
    let lines: Vec<&str> = source.lines().collect();
    let mut out = Vec::new();
    // Scope isolation. A script-style file's synthetic `<module>` entry
    // spans "first through last top-level statement line", so a real
    // `Function` declared BETWEEN two top-level statements sits textually
    // inside the module's range. Scanning the module would otherwise walk
    // that function's body under the MODULE's `closed` set — `Function` is
    // not in BRANCH_OR_LOOP, so nothing gates it — and flag an unrelated,
    // same-named, never-closed channel. Every line is therefore attributed
    // to its NARROWEST enclosing function and scanned only under that
    // owner, the same technique `match_missing_case_else_diagnostics` uses.
    let func_ranges: Vec<(usize, usize, usize)> = symbols
        .iter()
        .enumerate()
        .filter(|(_, s)| s.kind == "function")
        .map(|(i, s)| (i, s.start_line as usize, s.end_line as usize))
        .collect();

    for (sym_idx, sym) in symbols.iter().enumerate() {
        if sym.kind != "function" {
            continue;
        }
        let start = sym.start_line as usize;
        let end = (sym.end_line as usize).min(lines.len());
        if start == 0 || start > lines.len() || end < start {
            continue;
        }

        let mut closed: HashSet<String> = HashSet::new();
        let mut stack: Vec<&'static str> = Vec::new();
        // `closed` as it stood on entry to each currently-open `Try`.
        let mut try_snapshots: Vec<HashSet<String>> = Vec::new();
        for line_no in start..=end {
            // Skip the declaration line itself (`Function Foo(...) As R`):
            // nothing pushed for it, so its own body scan below never runs
            // against the header text.
            if line_no == start {
                continue;
            }
            // Lines owned by a narrower function are that function's to
            // scan, not this one's. Skipping them wholesale also keeps
            // `stack` balanced, since the nested `Function` opener and its
            // `End Function` are skipped as a pair.
            if func_ranges
                .iter()
                .filter(|(_, s, e)| *s <= line_no && line_no <= *e)
                .min_by_key(|(_, s, e)| e - s)
                .map(|(i, _, _)| *i)
                != Some(sym_idx)
            {
                continue;
            }
            let raw = lines[line_no - 1];
            let trimmed = ml_extractor::strip_comment(raw).trim();
            if trimmed.is_empty() {
                continue;
            }

            let closer: Option<String> = if trimmed == "End" {
                stack.last().map(|s| s.to_string())
            } else {
                ml_extractor::closes_block(trimmed)
            };
            if let Some(closed_kw) = closer {
                if stack
                    .last()
                    .map(|s| *s == closed_kw.as_str())
                    .unwrap_or(false)
                    && stack.pop() == Some("Try")
                {
                    try_snapshots.pop();
                }
                continue;
            }

            if let Some(kw) = ml_extractor::block_opener(trimmed) {
                if !ml_extractor::has_inline_closer(trimmed, kw) {
                    stack.push(kw);
                    if kw == "Try" {
                        try_snapshots.push(closed.clone());
                    }
                }
                continue;
            }

            // `Catch`/`Finally` are neither openers nor closers, so they
            // arrive here as plain statements. Both are reachable on a path
            // where an earlier statement in the `Try` body threw, which
            // means a `Close` recorded INSIDE that body may never have run.
            // Roll `closed` back to the Try's entry snapshot. A Close that
            // completed BEFORE the Try is already in that snapshot and
            // correctly survives — see the pair of tests covering both.
            if stack.last().map(|s| *s == "Try").unwrap_or(false)
                && (trimmed == "Catch" || trimmed.starts_with("Catch ") || trimmed == "Finally")
                && let Some(snap) = try_snapshots.last()
            {
                closed = snap.clone();
                continue;
            }

            let branch_depth = stack.iter().filter(|k| BRANCH_OR_LOOP.contains(k)).count();

            // `If cond Then Close(ch)` (MiniLang's one-line conditional
            // statement, see `ml_extractor::if_has_trailing_then_statement`)
            // is not a genuine block opener, so it falls through to here —
            // but a `Close` found in it is NOT unconditional and must not be
            // recorded. A `Send` found in it is still safe to check against
            // an ALREADY-recorded `Close`: that Close already unconditionally
            // happened, so a conditionally-reached Send after it is still a
            // real (if conditional) trap.
            let is_conditional_oneliner = trimmed
                .strip_prefix("If")
                .map(|after| {
                    after.starts_with(' ') && ml_extractor::if_has_trailing_then_statement(after)
                })
                .unwrap_or(false);

            if !is_conditional_oneliner
                && let Some(cap) = CLOSE_RE.captures(trimmed)
                && branch_depth == 0
            {
                closed.insert(cap[1].to_string());
            }
            if let Some(cap) = SEND_RE.captures(trimmed) {
                let chan = &cap[1];
                if branch_depth == 0 && closed.contains(chan) {
                    out.push(LanguageDiagnostic {
                        location: format!("{file}:{line_no}"),
                        category: "send_on_closed_channel".to_string(),
                        severity: "high".to_string(),
                        evidence: trimmed.to_string(),
                        guidance: format!(
                            "`{chan}` is Close()d earlier in this function body with no \
                             intervening branch or loop, so this Send is guaranteed to run \
                             against an already-closed channel — a runtime fault (exit code -5) \
                             when this line executes. Remove the stray Send, or restructure so \
                             Close only runs after every Send targeting this channel has run."
                        ),
                    });
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------
// Match without Case Else / Default over a union.
// ---------------------------------------------------------------------

/// Parameter `(name, type)` pairs from a declaration line's parameter
/// list, PRESERVING the type text. `ml_extractor::decls::parse_params`
/// deliberately discards the type (it only needs the binding mode and
/// name) — this is the one diagnostic that needs the type back, to
/// resolve a `Match` scrutinee that is a plain (unprefixed, owned)
/// parameter, so this is a small dedicated re-implementation rather than
/// changing that shared helper's output shape for every other caller.
fn params_with_types(trimmed: &str) -> Vec<(String, String)> {
    let Some(open) = decls::param_list_start(trimmed) else {
        return Vec::new();
    };
    let bytes = trimmed.as_bytes();
    let mut depth = 0i32;
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
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
            let rest = p
                .strip_prefix("BorrowMut ")
                .or_else(|| p.strip_prefix("Borrow "))
                .unwrap_or(p);
            let (name, ty) = rest.split_once(" As ")?;
            let name = name.trim().to_string();
            let ty: String = ty
                .trim()
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '.')
                .collect();
            if name.is_empty() || ty.is_empty() {
                None
            } else {
                Some((name, ty))
            }
        })
        .collect()
}

/// True for a non-empty, plain identifier — no dots, no calls, no
/// indexing. A `Match` scrutinee shaped any other way (`Match
/// self.Kind`, `Match GetShape()`, …) is not resolvable by this
/// line-scan-level diagnostic, and per the design brief it must be
/// skipped rather than guessed at.
fn is_simple_identifier(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// Split `s` on top-level commas only (parens balanced) — handles the
/// (unobserved in the real corpus for `Match`, but real for `Select`)
/// multi-label case-row shape `Case A, B`.
fn split_top_level_commas(s: &str) -> Vec<&str> {
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut out = Vec::new();
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                out.push(s[start..i].trim());
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    out.push(s[start..].trim());
    out
}

/// The `Case`-label variant names found DIRECTLY inside a `Match` block
/// (not inside a nested block, e.g. a guard's own `If`, or a nested
/// `Match`) starting at `open_idx` — the index, into `source.lines()`, of
/// the line that opened it — plus whether a `Case Else`/`Default`
/// catch-all exists anywhere directly inside it.
///
/// Reimplemented rather than reusing `ml_extractor::collect_block_body`:
/// that collector only tracks nesting of the SAME keyword (so it can
/// return a nested `Match`'s own `Case` rows flattened in with the outer
/// one's), which would either invent coverage that is not there or hide a
/// real gap. This tracks every nested block, and only records a `Case`
/// row when the stack is back down to depth 1 (directly inside THIS
/// Match).
fn match_case_coverage(source: &str, open_idx: usize) -> (Vec<String>, bool) {
    let mut labels = Vec::new();
    let mut has_default = false;
    let mut stack: Vec<&str> = vec!["Match"];

    for line in source.lines().skip(open_idx + 1) {
        let trimmed = ml_extractor::strip_comment(line).trim();
        if trimmed.is_empty() {
            continue;
        }
        let closer: Option<String> = if trimmed == "End" {
            stack.last().map(|s| s.to_string())
        } else {
            ml_extractor::closes_block(trimmed)
        };
        if let Some(closed_kw) = closer {
            if stack
                .last()
                .map(|s| *s == closed_kw.as_str())
                .unwrap_or(false)
            {
                stack.pop();
            }
            if stack.is_empty() {
                break;
            }
            continue;
        }
        if let Some(kw) = ml_extractor::block_opener(trimmed) {
            if !ml_extractor::has_inline_closer(trimmed, kw) {
                stack.push(kw);
            }
            continue;
        }
        if stack.len() == 1 {
            if trimmed == "Case Else" || trimmed.starts_with("Default") {
                has_default = true;
            } else if let Some(rest) = trimmed.strip_prefix("Case ") {
                for part in split_top_level_commas(rest.trim()) {
                    let vname: String = part
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .collect();
                    if !vname.is_empty() {
                        labels.push(vname);
                    }
                }
            }
        }
    }
    (labels, has_default)
}

/// `Match` is exhaustive in MiniLang; the compiler rejects a non-exhaustive
/// `Match` over a union unless a `Case Else`/`Default` clause exists. Flags
/// a `Match` whose `Case` labels do not cover every variant of the matched
/// union and which has no catch-all — the "you just added a variant and
/// forgot to update this Match" pre-edit risk.
///
/// Union resolution is FILE-SCOPED (see `strong_ref_cycle_diagnostics`'s
/// doc comment for why — the real corpus reuses short type names like
/// `Expr`/`Shape`/`Color` across unrelated files). The scrutinee's type is
/// resolved only when the `Match` expression is a plain identifier bound
/// by a `Dim`/`Var`/`Mut` local or a parameter (with or without a
/// `Borrow`/`BorrowMut` mode) of the ENCLOSING function — never a call, a
/// field/member access, or anything this diagnostic would have to guess
/// at; those are skipped, per the design brief.
fn match_missing_case_else_diagnostics(
    file: &str,
    source: &str,
    symbols: &[ExtractedSymbol],
) -> Vec<LanguageDiagnostic> {
    let mut unions: BTreeMap<String, Option<HashSet<String>>> = BTreeMap::new();
    for sym in symbols {
        if sym.kind != "union" {
            continue;
        }
        let bare = bare_name(&sym.name);
        let variants_str = sym
            .metadata
            .as_ref()
            .and_then(|m| m.get("variants"))
            .map(String::as_str)
            .unwrap_or("");
        let variant_names: HashSet<String> = variants_str
            .split("||")
            .filter(|s| !s.is_empty())
            .filter_map(|entry| entry.split_once('/').map(|(n, _)| n.to_string()))
            .collect();
        match unions.entry(bare) {
            Entry::Occupied(mut e) => {
                e.insert(None); // duplicate bare name in this file: ambiguous
            }
            Entry::Vacant(e) => {
                e.insert(Some(variant_names));
            }
        }
    }

    let functions: Vec<&ExtractedSymbol> =
        symbols.iter().filter(|s| s.kind == "function").collect();
    let source_lines: Vec<&str> = source.lines().collect();
    let mut out = Vec::new();

    for (idx, raw_line) in source.lines().enumerate() {
        let trimmed = ml_extractor::strip_comment(raw_line).trim();
        if ml_extractor::block_opener(trimmed) != Some("Match") {
            continue;
        }
        // A one-line self-closing Match (never observed in the real
        // corpus, and an implausible shape given Match's multi-line Case
        // arms) is skipped rather than risking `match_case_coverage`
        // scanning past it looking for an `End Match` that already
        // happened on this same line.
        if ml_extractor::has_inline_closer(trimmed, "Match") {
            continue;
        }
        let line_no = (idx + 1) as u32;
        let expr = trimmed.strip_prefix("Match").map(str::trim).unwrap_or("");
        if !is_simple_identifier(expr) {
            continue;
        }

        let (labels, has_default) = match_case_coverage(source, idx);
        if has_default {
            continue;
        }

        let Some(func) = functions
            .iter()
            .filter(|f| f.start_line <= line_no && line_no <= f.end_line)
            .min_by_key(|f| f.end_line - f.start_line)
        else {
            continue;
        };
        let is_synthetic = func
            .metadata
            .as_ref()
            .and_then(|m| m.get("synthetic"))
            .is_some_and(|s| s == "module_entry");

        let body_start = (func.start_line as usize).saturating_sub(1);
        let body_end = (line_no as usize)
            .saturating_sub(1)
            .max(body_start)
            .min(source_lines.len());
        let body_before = if body_start < body_end {
            source_lines[body_start..body_end].join("\n")
        } else {
            String::new()
        };

        // Locals shadow an eponymous parameter (rare, but a later
        // declaration should win); keep the LAST textual match.
        let local_ty = LOCAL_TYPE_RE
            .captures_iter(&body_before)
            .filter(|c| &c[1] == expr)
            .last()
            .map(|c| c[2].to_string());
        let param_ty = if is_synthetic {
            // The synthetic `<module>` entry's "header" is just its first
            // top-level statement, not a real parameter list — calling
            // `params_with_types` on it would misparse an unrelated `(`.
            None
        } else {
            let header = source_lines.get(body_start).copied().unwrap_or("");
            params_with_types(header)
                .into_iter()
                .find(|(n, _)| n == expr)
                .map(|(_, t)| t)
        };
        let Some(ty) = local_ty.or(param_ty) else {
            continue;
        };

        let bare_ty = bare_name(&ty);
        let Some(Some(variants)) = unions.get(&bare_ty) else {
            continue; // not a union in this file, or an ambiguous bare name
        };
        let covered: HashSet<&str> = labels.iter().map(String::as_str).collect();
        let mut missing: Vec<&str> = variants
            .iter()
            .filter(|v| !covered.contains(v.as_str()))
            .map(String::as_str)
            .collect();
        if missing.is_empty() {
            continue;
        }
        missing.sort_unstable();
        let missing_list = missing.join(", ");

        out.push(LanguageDiagnostic {
            location: format!("{file}:{line_no}"),
            category: "match_missing_case_else".to_string(),
            severity: "high".to_string(),
            evidence: format!("Match {expr} ({bare_ty}) — uncovered: {missing_list}"),
            guidance: format!(
                "MiniLang Match is exhaustive; the compiler rejects a non-exhaustive Match with \
                 no catch-all. `{bare_ty}` has variant(s) not handled by any Case arm here \
                 ({missing_list}), and this Match has no `Case Else`/`Default`. Add the missing \
                 Case arm(s), or a Case Else/Default to handle them explicitly."
            ),
        });
    }
    out
}

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

    #[test]
    fn flags_a_direct_strong_ref_self_cycle() {
        // Real corpus shape: tests/conformance/arc/test_mlh2370_strong_cycle_leak_evidence.ml
        let src = "\
Type StrongCycleNode
    Peer As Ref(Of StrongCycleNode)
End Type
";
        let files = vec![("cycle.ml", src)];
        let out = detect(&files);
        let hit = out
            .iter()
            .find(|d| d.category == "strong_ref_self_cycle")
            .unwrap_or_else(|| panic!("expected a strong_ref_self_cycle finding, got {out:?}"));
        assert_eq!(hit.location, "cycle.ml:1");
        assert!(hit.evidence.contains("StrongCycleNode"), "{}", hit.evidence);
    }

    #[test]
    fn flags_a_transitive_strong_ref_cycle() {
        let src = "\
Type A
    Next As Ref(Of B)
End Type

Type B
    Back As Ref(Of A)
End Type
";
        let files = vec![("transitive.ml", src)];
        let out = detect(&files);
        let cycle_hits: Vec<_> = out
            .iter()
            .filter(|d| d.category == "strong_ref_self_cycle")
            .collect();
        assert_eq!(
            cycle_hits.len(),
            2,
            "both A and B should be flagged: {out:?}"
        );
    }

    #[test]
    fn weak_or_nullable_back_edge_does_not_cycle() {
        // Real corpus shape: tests/conformance/arc/test_mlh2370_weak_cycle_shapes.ml —
        // a 2-hop pair broken by a Weak edge on one side must not fire.
        let weak_src = "\
Type CycleChild
    Value As Int
    Parent As Weak(Of CycleParent)
End Type

Type CycleParent
    Value As Int
    Child As Ref(Of CycleChild)
End Type
";
        let out = detect(&[("weak.ml", weak_src)]);
        assert!(
            out.iter().all(|d| d.category != "strong_ref_self_cycle"),
            "a Weak back-edge must break the cycle: {out:?}"
        );

        let nullable_src = "\
Type Node
    Next As Ref(Of Node)?
End Type
";
        let out = detect(&[("nullable.ml", nullable_src)]);
        assert!(
            out.iter().all(|d| d.category != "strong_ref_self_cycle"),
            "a nullable back-edge must break the cycle: {out:?}"
        );
    }

    #[test]
    fn flags_send_after_close_same_function() {
        // Real corpus shape: tests/conformance/channels-typed/test_send_on_closed_channel_traps.ml
        let src = "\
Function Boot() As Int
    Var ch As Channel(Of Int) = NewChannel(Of Int)(2)
    Close(ch)
    Send(ch, 3)
    Return 0
End Function
";
        let out = detect(&[("closed.ml", src)]);
        let hit = out
            .iter()
            .find(|d| d.category == "send_on_closed_channel")
            .unwrap_or_else(|| panic!("expected a send_on_closed_channel finding, got {out:?}"));
        assert_eq!(hit.location, "closed.ml:4");
    }

    #[test]
    fn flags_send_after_close_across_a_try_boundary() {
        // Real corpus shape: tests/conformance/channels-close/test_mlh2360_send_closed_caught.ml —
        // Close happens unconditionally, then the risky Send is wrapped in
        // Try/Catch to handle the resulting fault. Try is transparent to
        // this diagnostic (see BRANCH_OR_LOOP's doc comment), so this must
        // still fire: the Send genuinely does target an already-closed
        // channel, whether or not the fault is caught.
        let src = "\
Function Boot() As Int
    Var ch As Channel(Of Int) = NewChannel(Of Int)(1)
    Close(ch)
    Try
        Send(ch, 42)
    Catch fault As Std.Errors.RuntimeFault
        Say fault.code
    End Try
    Return 0
End Function
";
        let out = detect(&[("caught.ml", src)]);
        assert!(
            out.iter().any(|d| d.category == "send_on_closed_channel"),
            "expected a send_on_closed_channel finding through a Try boundary: {out:?}"
        );
    }

    #[test]
    fn module_scope_close_does_not_leak_into_a_nested_function() {
        // A script-style file's synthetic `<module>` entry spans "first
        // through last top-level statement", so a `Function` declared
        // BETWEEN two top-level statements sits textually inside it.
        // `Function` is not in BRANCH_OR_LOOP, so without explicit scope
        // isolation the module's scan walks straight through the function
        // body at branch_depth 0 and flags this `Send` — even though the
        // function's `ch` is a different, never-closed channel.
        //
        // The corpus already contains this exact layout
        // (tests/stress/fibers/race_close_vs_send.ml: a top-level `Var ch`,
        // a `Function` between it and a later `Close(ch)`); it escapes the
        // fault only because that function's parameter is named `c`.
        let src = "\
Var ch As Channel(Of Int) = NewChannel(Of Int)(1)
Close(ch)

Function Helper() As Int
    Var ch As Channel(Of Int) = NewChannel(Of Int)(2)
    Send(ch, 1)
    Return 0
End Function

Return 0
";
        let out = detect(&[("script.ml", src)]);
        assert!(
            out.iter().all(|d| d.category != "send_on_closed_channel"),
            "a nested function's own `ch` must not be judged against the \
             module scope's closed set: {out:?}"
        );
    }

    #[test]
    fn close_inside_try_then_send_in_catch_is_not_flagged() {
        // The `Catch` clause is reached only when something in the `Try`
        // body threw — which means a `Close` recorded INSIDE that body may
        // never have executed. Treating Try as transparent is right for a
        // Close that completed BEFORE the Try (see the test above), but
        // not for one recorded inside it.
        let src = "\
Function Boot() As Int
    Var ch As Channel(Of Int) = NewChannel(Of Int)(1)
    Try
        Close(ch)
    Catch fault As Std.Errors.RuntimeFault
        Send(ch, 2)
    End Try
    Return 0
End Function
";
        let out = detect(&[("catch.ml", src)]);
        assert!(
            out.iter().all(|d| d.category != "send_on_closed_channel"),
            "a Close inside the Try body may not have run on the path that \
             reaches Catch: {out:?}"
        );
    }

    #[test]
    fn close_before_try_still_flags_a_send_in_catch() {
        // Discriminator for the test above: this Close is unconditional and
        // completes before the Try is entered, so it HAS happened on every
        // path that reaches the Catch. Forgetting Try-body closes must not
        // also forget this one.
        let src = "\
Function Boot() As Int
    Var ch As Channel(Of Int) = NewChannel(Of Int)(1)
    Close(ch)
    Try
        Say \"risky\"
    Catch fault As Std.Errors.RuntimeFault
        Send(ch, 2)
    End Try
    Return 0
End Function
";
        let out = detect(&[("precatch.ml", src)]);
        assert!(
            out.iter().any(|d| d.category == "send_on_closed_channel"),
            "a Close completed before the Try still governs the Catch: {out:?}"
        );
    }

    #[test]
    fn duplicate_struct_name_in_one_file_drops_the_cycle_candidate() {
        // File-scoped name resolution deliberately DROPS an ambiguous bare
        // name rather than guessing which declaration a field refers to.
        // Locks in the fail-safe direction (miss, never false positive).
        let src = "\
Type Node
    Next As Ref(Of Node)
End Type

Type Node
    Value As Int
End Type
";
        let out = detect(&[("dup.ml", src)]);
        assert!(
            out.iter().all(|d| d.category != "strong_ref_self_cycle"),
            "an ambiguous struct name must be dropped, not guessed: {out:?}"
        );
    }

    #[test]
    fn send_in_a_different_branch_than_close_is_not_flagged() {
        let src = "\
Function Boot(cond As Bool) As Int
    Var ch As Channel(Of Int) = NewChannel(Of Int)(1)
    If cond Then
        Close(ch)
    Else
        Send(ch, 1)
    End If
    Return 0
End Function
";
        let out = detect(&[("branchy.ml", src)]);
        assert!(
            out.iter().all(|d| d.category != "send_on_closed_channel"),
            "Close and Send in mutually exclusive branches must not fire: {out:?}"
        );
    }

    #[test]
    fn send_before_close_is_not_flagged() {
        let src = "\
Function Boot() As Int
    Var ch As Channel(Of Int) = NewChannel(Of Int)(1)
    Send(ch, 1)
    Close(ch)
    Return 0
End Function
";
        let out = detect(&[("ok_order.ml", src)]);
        assert!(
            out.iter().all(|d| d.category != "send_on_closed_channel"),
            "a Send BEFORE the Close must not fire: {out:?}"
        );
    }

    #[test]
    fn flags_match_missing_a_variant_with_no_default() {
        // Real corpus shape: tests/negative/semantic/match_nonexhaustive.ml
        let src = "\
Type Color
    Red
    Green
    Blue
End Type

Function Describe(c As Color) As Int
    Match c
    Case Red
        Return 1
    Case Green
        Return 2
    End Match
    Return 0
End Function
";
        let out = detect(&[("nonexh.ml", src)]);
        let hit = out
            .iter()
            .find(|d| d.category == "match_missing_case_else")
            .unwrap_or_else(|| panic!("expected a match_missing_case_else finding, got {out:?}"));
        assert!(hit.evidence.contains("Blue"), "{}", hit.evidence);
    }

    #[test]
    fn match_with_default_is_not_flagged_even_if_partial() {
        let src = "\
Type Color
    Red
    Green
    Blue
End Type

Function Describe(c As Color) As Int
    Match c
    Case Red
        Return 1
    Case Else
        Return 0
    End Match
End Function
";
        let out = detect(&[("hasdefault.ml", src)]);
        assert!(
            out.iter().all(|d| d.category != "match_missing_case_else"),
            "a Case Else catch-all must suppress the finding: {out:?}"
        );
    }

    #[test]
    fn fully_covered_match_is_not_flagged() {
        let src = "\
Type Color
    Red
    Green
    Blue
End Type

Function Describe(c As Color) As Int
    Match c
    Case Red
        Return 1
    Case Green
        Return 2
    Case Blue
        Return 3
    End Match
End Function
";
        let out = detect(&[("covered.ml", src)]);
        assert!(
            out.iter().all(|d| d.category != "match_missing_case_else"),
            "a fully-covered Match must not fire: {out:?}"
        );
    }

    #[test]
    fn match_over_an_unresolvable_scrutinee_is_skipped_not_guessed() {
        // `GetColor()` is a call, not a bare identifier -- this diagnostic
        // must not guess at its type.
        let src = "\
Type Color
    Red
    Green
    Blue
End Type

Function Describe() As Int
    Match GetColor()
    Case Red
        Return 1
    End Match
    Return 0
End Function
";
        let out = detect(&[("unresolvable.ml", src)]);
        assert!(
            out.iter().all(|d| d.category != "match_missing_case_else"),
            "an unresolvable scrutinee must be skipped, not guessed at: {out:?}"
        );
    }
}
