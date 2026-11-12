use engram_core::go_modules::{Config, Replacement};
use std::collections::BTreeMap;

/// Go interpreted/raw strings, not host paths or shell text.
pub(super) fn unquote(raw: &str) -> Option<String> {
    if let Some(value) = raw.strip_prefix('`').and_then(|v| v.strip_suffix('`')) {
        return Some(value.replace('\r', ""));
    }
    let value = raw.strip_prefix('"')?.strip_suffix('"')?;
    let mut chars = value.chars();
    let mut bytes = Vec::new();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut b = [0; 4];
            bytes.extend(c.encode_utf8(&mut b).as_bytes());
            continue;
        }
        let c = chars.next()?;
        let simple = match c {
            'a' => Some(7),
            'b' => Some(8),
            'f' => Some(12),
            'n' => Some(10),
            'r' => Some(13),
            't' => Some(9),
            'v' => Some(11),
            '\\' => Some(b'\\'),
            '"' => Some(b'"'),
            _ => None,
        };
        if let Some(b) = simple {
            bytes.push(b);
            continue;
        }
        let (digits, radix, mut number) = match c {
            'x' => (2, 16, 0),
            'u' => (4, 16, 0),
            'U' => (8, 16, 0),
            '0'..='7' => (2, 8, c.to_digit(8)?),
            _ => return None,
        };
        for _ in 0..digits {
            number = number
                .checked_mul(radix)?
                .checked_add(chars.next()?.to_digit(radix)?)?;
        }
        if matches!(c, 'u' | 'U') {
            let mut b = [0; 4];
            bytes.extend(char::from_u32(number)?.encode_utf8(&mut b).as_bytes());
        } else {
            bytes.push(u8::try_from(number).ok()?);
        }
    }
    String::from_utf8(bytes).ok()
}
fn tokens(line: &str) -> Option<Vec<String>> {
    let mut out = vec![];
    let mut i = 0;
    while i < line.len() {
        let rest = &line[i..];
        let c = rest.chars().next()?;
        if c.is_whitespace() {
            i += c.len_utf8();
            continue;
        }
        if rest.starts_with("//") {
            break;
        }
        if matches!(c, '(' | ')') {
            out.push(c.to_string());
            i += 1;
            continue;
        }
        if rest.starts_with("=>") {
            out.push("=>".into());
            i += 2;
            continue;
        }
        if c == '"' || c == '`' {
            let start = i;
            i += 1;
            let mut closed = false;
            while i < line.len() {
                let n = line[i..].chars().next()?;
                i += n.len_utf8();
                if n == c {
                    closed = true;
                    break;
                }
                if n == '\\' && c == '"' {
                    i += line[i..].chars().next()?.len_utf8();
                }
            }
            if !closed {
                return None;
            }
            out.push(unquote(&line[start..i])?);
        } else {
            let start = i;
            while i < line.len() {
                let n = line[i..].chars().next()?;
                if n.is_whitespace()
                    || matches!(n, '(' | ')')
                    || line[i..].starts_with("//")
                    || line[i..].starts_with("=>")
                {
                    break;
                }
                if matches!(n, ';' | '"' | '`') {
                    return None;
                }
                i += n.len_utf8();
            }
            if i == start {
                return None;
            }
            out.push(line[start..i].into());
        }
        if out.len() > 1024 {
            return None;
        }
    }
    Some(out)
}
pub(crate) fn extract(source: &str, workspace: bool) -> Config {
    let mut facts = Config {
        version: 1,
        module: None,
        requires: BTreeMap::new(),
        replaces: vec![],
        uses: vec![],
        issues: vec![],
    };
    let mut block: Option<String> = None;
    for (line, raw) in source.lines().enumerate() {
        if line >= 100_000 {
            facts.issues.push("config_budget_exceeded".into());
            break;
        }
        let Some(mut row) = tokens(raw) else {
            facts.issues.push("invalid_module_config".into());
            continue;
        };
        if row.is_empty() {
            continue;
        }
        if row == [")"] {
            if block.take().is_none() {
                facts.issues.push("invalid_module_config".into());
            }
            continue;
        }
        let directive = if let Some(name) = &block {
            name.clone()
        } else {
            row.remove(0)
        };
        if row == ["("] && block.is_none() {
            block = Some(directive);
            continue;
        }
        let valid = match directive.as_str() {
            "module" if !workspace => {
                if row.len() == 1 && facts.module.is_none() {
                    facts.module = Some(row[0].clone());
                    true
                } else {
                    false
                }
            }
            "require" if !workspace => {
                row.len() == 2
                    && facts
                        .requires
                        .insert(row[0].clone(), row[1].clone())
                        .is_none()
            }
            "use" if workspace => {
                if row.len() == 1 {
                    facts.uses.push(row[0].clone());
                    true
                } else {
                    false
                }
            }
            "replace" => {
                if let Some(at) = row.iter().position(|t| t == "=>") {
                    if matches!(at, 1 | 2) && matches!(row.len() - at - 1, 1 | 2) {
                        facts.replaces.push(Replacement {
                            module: row[0].clone(),
                            version: (at == 2).then(|| row[1].clone()),
                            destination: row[at + 1].clone(),
                            destination_version: row.get(at + 2).cloned(),
                        });
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
            // These do not declare package paths. Exclusions are retained as
            // unsupported: determining their selected versions needs MVS.
            "go" | "toolchain" | "godebug" | "retract" | "tool" | "ignore" => !row.is_empty(),
            "exclude" => {
                facts.issues.push("excluded_versions_not_evaluated".into());
                true
            }
            _ => false,
        };
        if !valid {
            facts
                .issues
                .push("invalid_or_unsupported_module_config".into());
        }
    }
    if block.is_some() {
        facts.issues.push("unterminated_module_config".into());
    }
    if !workspace && facts.module.is_none() {
        facts.issues.push("missing_module_directive".into());
    }
    facts.issues.sort();
    facts.issues.dedup();
    facts
}
