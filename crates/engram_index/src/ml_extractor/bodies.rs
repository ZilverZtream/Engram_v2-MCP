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
static SIMD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^Std\.Vector\.[A-Za-z]+(128|256|512)$").expect("valid SIMD regex")
});

/// Keywords that read as calls but are language constructs or control
/// flow — not edges into project code.
const CALL_STOPWORDS: &[&str] = &[
    "if",
    "elseif",
    "while",
    "for",
    // `Try Call X(...)` / `Try X(...)` (a single-line fallible-call
    // statement, real corpus: `Std.Collections.Deque.ml:676`) now falls
    // through to this statement scanner instead of opening a block (see
    // `block_opener`'s `Try` handling in `mod.rs`) — the two real corpus
    // shapes always have a space before the call target, so `Try` itself
    // never abuts `(` and CALL_RE never matches it, but this stopword is
    // added defensively for symmetry with every other control-flow
    // keyword in this list.
    "try",
    "return",
    "set",
    "say",
    "var",
    "dim",
    "mut",
    "const",
    "throw",
    "case",
    "match",
    "catch",
    "using",
    "unsafe",
    "asm",
    "type",
    "function",
    "sub",
    "namespace",
    "enum",
    "interface",
    "include",
    "declare",
    "extern",
    "spawn",
    "call",
    "and",
    "or",
    "not",
];

/// Generic TYPE constructors. `Var v As Vector256(Of Int32)` is a type
/// annotation, not a call — without this the graph fills with calls to
/// `Vector256`, `Channel`, and `List` that no function ever makes. Also
/// covers the `New <Type>(…)` construction shape (`New Ref(Of T)(…)`,
/// `New Slice(Of T)(…)`), where the type name follows `New`, not `As`, so
/// the type-annotation-position guard below does not fire and this list is
/// the ONLY thing suppressing the phantom edge.
const TYPE_CONSTRUCTORS: &[&str] = &[
    "Channel",
    "Vector128",
    "Vector256",
    "Vector512",
    "Ref",
    "Weak",
    "Atomic",
    "List",
    "SoA",
    "Function",
    "Option",
    "Result",
    "Slice",
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
    let rest = rest
        .strip_prefix("Call ")
        .map(str::trim_start)
        .unwrap_or(rest);

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

        let generic_args = cap
            .get(2)
            .map(|m| m.as_str().trim().to_string())
            .unwrap_or_default();

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
            source_start_line: line_no,
            source_language: "ml".to_string(),
            target_name: name.to_string(),
            target_kind: Some("function".to_string()),
            target_start_line: None,
            kind: "calls".to_string(),
            metadata: meta(&[
                (
                    "spawn",
                    if spawn {
                        "true".to_string()
                    } else {
                        String::new()
                    },
                ),
                (
                    "detached",
                    if detached {
                        "true".to_string()
                    } else {
                        String::new()
                    },
                ),
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
