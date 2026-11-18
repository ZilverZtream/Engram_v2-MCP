//! Additional positive declarations for captured instruction evidence only.
//! This never changes the live index or starts the semantic sidecar.
use super::{ExtractedSymbol, LazyLock, Regex};
use std::collections::HashMap;

static PROPERTY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)^(?:<(?:[^>"]|"(?:[^"]|"")*")*>\s*|(?:Public|Protected|Friend|Private|Shared|Shadows|Overloads|Overrides|Overridable|NotOverridable|MustOverride|Default|ReadOnly|WriteOnly)\s+)*Property\s+(\[[A-Za-z_]\w*\]|[A-Za-z_]\w*)"#)
        .expect("valid captured VB property declaration")
});
static SCOPE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:(?:Public|Protected|Friend|Private|Partial|MustInherit|NotInheritable)\s+)*(Namespace|Class|Module|Structure|Interface|Enum)\s+([A-Za-z_][\w.]*)")
        .expect("valid captured VB scope declaration")
});
static ACCESSOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:(?:Public|Protected|Friend|Private)\s+)*(?:Get|Set)\b")
        .expect("valid captured VB property accessor")
});

pub(super) fn properties(source: &str) -> Vec<ExtractedSymbol> {
    let lines: Vec<_> = source.lines().collect();
    let mut scopes: Vec<(String, String)> = Vec::new();
    let mut result = Vec::new();
    for (index, raw) in lines.iter().enumerate() {
        let line = raw.trim();
        let lower = line.to_ascii_lowercase();
        if let Some(kind) = lower.strip_prefix("end ") {
            if let Some((open, _)) = scopes.last()
                && kind.split_whitespace().next() == Some(open.as_str())
            {
                scopes.pop();
            }
            continue;
        }
        if let Some(scope) = SCOPE.captures(line) {
            scopes.push((scope[1].to_ascii_lowercase(), scope[2].to_owned()));
            continue;
        }
        let Some(declaration) = PROPERTY.captures(line) else {
            continue;
        };
        let name = declaration[1].trim_matches(['[', ']']);
        let qualified = scopes
            .iter()
            .map(|(_, name)| name.as_str())
            .chain(std::iter::once(name))
            .collect::<Vec<_>>()
            .join(".");
        let next = lines[index + 1..]
            .iter()
            .position(|line| !line.trim().is_empty() && !line.trim().starts_with('\''))
            .map(|offset| index + 1 + offset);
        let has_accessors = next.is_some_and(|next| ACCESSOR.is_match(lines[next].trim()));
        let mut end = index;
        let mut span_kind = "regex_declaration_line_only";
        if has_accessors {
            // Refuse a body range across another member/type when End Property
            // is absent. A declaration candidate remains useful but incomplete.
            for (offset, raw) in lines[index + 1..].iter().enumerate() {
                let body = raw.trim();
                let lower = body.to_ascii_lowercase();
                if lower == "end property" || lower.starts_with("end property '") {
                    end = index + 1 + offset;
                    span_kind = "regex_observed_property_line_range";
                    break;
                }
                if PROPERTY.is_match(body)
                    || SCOPE.is_match(body)
                    || super::RE_VB_METHOD_DECL.is_match(body)
                    || scopes
                        .last()
                        .is_some_and(|(kind, _)| lower == format!("end {kind}"))
                {
                    break;
                }
            }
        } else if declaration.get(1).is_some_and(|name| {
            let tail = line[name.end()..].trim_start().to_ascii_lowercase();
            tail.starts_with("as ") || tail.starts_with('=')
        }) && !lower.ends_with('_')
            && !lower.split_whitespace().any(|word| word == "mustoverride")
            && !scopes.iter().any(|(kind, _)| kind == "interface")
        {
            span_kind = "regex_observed_auto_property_line_range";
        }
        result.push(ExtractedSymbol {
            name: qualified.clone(),
            kind: "property".into(),
            start_line: index as u32 + 1,
            end_line: end as u32 + 1,
            metadata: Some(HashMap::from([
                ("fqn".into(), qualified),
                ("snapshot_span_kind".into(), span_kind.into()),
                ("extraction_fallback".into(), "true".into()),
            ])),
        });
    }
    result
}

// An explicit access modifier distinguishes these type/module declarations
// from method-local Const statements. These remain positive source candidates.
static CONSTANT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:Public|Protected|Friend|Private)\s+(?:(?:Protected|Friend|Shadows)\s+)*Const\s+(\[[A-Za-z_]\w*\]|[A-Za-z_]\w*)(?:\s+As\s+[A-Za-z_][\w.]*)?\s*=\s*.+$")
        .expect("valid captured VB constant declaration")
});

pub(super) fn constants(source: &str) -> Vec<ExtractedSymbol> {
    let mut scopes: Vec<(String, String)> = Vec::new();
    let mut result = Vec::new();
    for (index, raw) in source.lines().enumerate() {
        let line = raw.trim();
        let lower = line.to_ascii_lowercase();
        if let Some(kind) = lower.strip_prefix("end ") {
            if let Some((open, _)) = scopes.last()
                && kind.split_whitespace().next() == Some(open.as_str()) {
                scopes.pop();
            }
            continue;
        }
        if let Some(scope) = SCOPE.captures(line) {
            scopes.push((scope[1].to_ascii_lowercase(), scope[2].to_owned()));
            continue;
        }
        // Explicit continuation is not complete one-line evidence. No absence
        // is inferred from unsupported declarations or this bounded scan.
        if !scopes.iter().any(|(kind, _)| matches!(kind.as_str(), "class" | "module" | "structure"))
            || scopes.iter().any(|(kind, _)| matches!(kind.as_str(), "enum" | "interface"))
            || line.split('\'').next().unwrap_or(line).trim_end().ends_with('_') {
            continue;
        }
        let Some(declaration) = CONSTANT.captures(line) else { continue; };
        let name = declaration[1].trim_matches(['[', ']']);
        let qualified = scopes.iter().map(|(_, name)| name.as_str())
            .chain(std::iter::once(name)).collect::<Vec<_>>().join(".");
        result.push(ExtractedSymbol {
            name: qualified.clone(), kind: "constant".into(),
            start_line: index as u32 + 1, end_line: index as u32 + 1,
            metadata: Some(HashMap::from([
                ("fqn".into(), qualified),
                ("snapshot_span_kind".into(), "regex_observed_constant_line_range".into()),
                ("extraction_fallback".into(), "true".into()),
            ])),
        });
    }
    result
}
