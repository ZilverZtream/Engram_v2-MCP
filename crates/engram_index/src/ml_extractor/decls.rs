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
    body: &[&str],
    symbols: &mut Vec<ExtractedSymbol>,
    edges: &mut Vec<ExtractedEdge>,
) -> (String, Option<usize>) {
    match keyword {
        "Namespace" => {
            let Some(name) = declaration_name(trimmed, "Namespace") else {
                return (String::new(), None);
            };
            let fqn = qualify(parent_fqn, &name);
            symbols.push(ExtractedSymbol {
                name: fqn.clone(),
                kind: "namespace".to_string(),
                start_line: line_no,
                end_line: 0,
                metadata: None,
            });
            (fqn, Some(symbols.len() - 1))
        }
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
                    (
                        "nullable_return",
                        if nullable {
                            "true".to_string()
                        } else {
                            String::new()
                        },
                    ),
                    ("throws", throws),
                ]),
            });
            (fqn, Some(symbols.len() - 1))
        }
        "Type" => {
            let Some(name) = declaration_name(trimmed, keyword) else {
                return (String::new(), None);
            };
            let fqn = qualify(parent_fqn, &name);

            // A row shaped `Name As Type` is a struct field; `Name(…)` or a
            // bare `Name` is a union variant. A `Type` is a union only when
            // it has at least one variant-shaped row and NO field rows; any
            // field row makes it a struct: a single field row forces
            // `kind: "struct"` even alongside variant-shaped rows, and any
            // such rows are still recorded in the `variants` metadata. This
            // is a defined fallback, not a dominance/majority rule.
            //
            // CORRECTION (this was previously documented, incorrectly, as
            // "mixed bodies never occur in valid MiniLang — the corpus has
            // zero such cases"): mixed-LOOKING bodies DO occur, via MLH-2080
            // inline `Type` methods (real corpus: `Type BuildJob` in
            // `tests/conformance/interfaces/test_mlh2080_type_inline_methods.ml`
            // has 2 fields plus 3 methods, and 10 files corpus-wide have this
            // shape once `tests/`/`docs/` are included in the scan — the
            // original zero-count measurement scoped only `src`+`examples`+
            // `benchmarks` and missed every one of them). The CALLER
            // (`mod.rs`'s `collect_type_member_rows`, not this function)
            // already excludes a method's declaration line and its entire
            // body from `body` before it ever reaches this loop — so `body`
            // here never actually contains a real method's rows, and this
            // fallback exists for TRUE field+variant mixing, which remains
            // unverified either way (not proven zero, not proven nonzero).
            let mut fields: Vec<String> = Vec::new();
            let mut variants: Vec<String> = Vec::new();
            for row in body {
                if let Some((lhs, rhs)) = row.split_once(" As ") {
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
                        if args.is_empty() {
                            0
                        } else {
                            args.matches(',').count() + 1
                        }
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
        _ => (String::new(), None),
    }
}

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
        if trimmed.starts_with(modifier) && trimmed[modifier.len()..].starts_with(' ') {
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

/// The project-relative target of an `Include "…"` line, or `None` when the
/// line names no legitimate project-relative file.
///
/// Include paths resolve relative to the INCLUDING file's directory. The
/// result stays project-relative: absolute edge targets are rejected by the
/// ingest safety check. `None` is the correct outcome — not a
/// best-effort fallback — for two adversarial shapes the corpus's
/// `tests/negative/includes/` suite exists specifically to prove the
/// MiniLang compiler itself rejects:
///   - absolute, UNC (`\\server\share\…`), or device-namespace (`\\?\…`)
///     paths, including alternate-data-stream targets (`C:\…:hidden`),
///     which are never project-relative to begin with;
///   - `..` segments that pop past the project root. Before this guard,
///     `parts.pop()` on an empty `Vec` was a silent no-op, so extra
///     leading `..`s were simply dropped and the function returned a
///     plausible-looking but FABRICATED in-project target instead of no
///     edge at all — worse than a missing edge.
pub(crate) fn include_target(trimmed: &str, rel_path: &str) -> Option<String> {
    let rest = trimmed.strip_prefix("Include")?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    let raw = rest[..end].trim().replace('\\', "/");
    if raw.is_empty() || is_unsafe_include_path(&raw) {
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
                // A `..` with nothing left to pop escapes the project
                // root: there is no legitimate project-relative target,
                // so bail out entirely rather than dropping the segment.
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

/// True for a path that can never resolve to a project-relative file:
/// UNC/device-namespace (`//server/…`, `//?/…` after backslash
/// normalisation), POSIX-absolute (`/…`), or drive-letter-absolute
/// (`C:/…`, which also covers alternate-data-stream targets like
/// `C:/Windows/win.ini:hidden`). `raw` must already have had `\` -> `/`
/// normalisation applied.
fn is_unsafe_include_path(raw: &str) -> bool {
    if raw.starts_with('/') {
        return true;
    }
    let bytes = raw.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// A `Declare Function … Lib "…"` (P/Invoke) or
/// `Extern "C" [Blocking] Function … Lib "…"` (C-FFI) binding.
///
/// A leading `Public`/`Private` modifier (4 corpus occurrences) is stripped
/// the same way `declaration_name` strips one, and recorded in the `access`
/// metadata for consistency with the other declaration kinds. Without this,
/// `Public Declare …` / `Public Extern …` lines matched neither this
/// function nor any other, and silently produced no symbol at all.
pub(crate) fn parse_ffi_binding(trimmed: &str, line_no: u32) -> Option<ExtractedSymbol> {
    let access = access_modifier(trimmed);
    let mut rest = trimmed;
    for modifier in ["Public ", "Private "] {
        if let Some(r) = rest.strip_prefix(modifier) {
            rest = r.trim_start();
        }
    }

    let (binding, rest) = if let Some(r) = rest.strip_prefix("Declare ") {
        ("pinvoke", r)
    } else if let Some(r) = rest.strip_prefix("Extern ") {
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
            ("access", access),
            (
                "blocking",
                if blocking {
                    "true".to_string()
                } else {
                    String::new()
                },
            ),
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

/// A `Const NAME = expr` declaration. Constants are CTFE-evaluated and
/// allocate no runtime storage, so the expression text is the useful
/// payload — it is what a fixed-array size resolves to.
///
/// A leading `Public`/`Private` modifier is stripped the same way
/// `declaration_name` strips one (no corpus occurrences today, but stripped
/// for symmetry with every other declaration kind), and recorded in the
/// `access` metadata.
pub(crate) fn parse_const(
    trimmed: &str,
    parent_fqn: &str,
    line_no: u32,
) -> Option<ExtractedSymbol> {
    let access = access_modifier(trimmed);
    let mut rest = trimmed;
    for modifier in ["Public ", "Private "] {
        if let Some(r) = rest.strip_prefix(modifier) {
            rest = r.trim_start();
        }
    }
    let rest = rest.strip_prefix("Const")?;
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
        metadata: meta(&[("value", value.trim().to_string()), ("access", access)]),
    })
}
