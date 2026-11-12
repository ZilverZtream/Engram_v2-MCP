use serde::Serialize;
use std::collections::BTreeMap;

/// Caller supplies an explicitly selected source inventory, never arbitrary disk paths.
#[derive(Clone, Copy)]
pub struct Source<'a> {
    pub path: &'a str,
    pub content: &'a str,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct RouteCandidate {
    pub page: String,
    pub route_name: Option<String>,
    pub pattern: String,
    pub parameters: BTreeMap<String, String>,
    pub source_path: String,
    pub source_line: usize,
    pub source_blake3: String,
    pub evidence: String,
    pub basis: &'static str,
}

#[derive(Debug, Serialize)]
pub struct Routing {
    pub request_path: String,
    pub method: Option<String>,
    pub candidates: Vec<RouteCandidate>,
    pub gaps: Vec<String>,
    pub interpretation: &'static str,
}

#[derive(Debug)]
pub(super) struct Token {
    pub text: String,
    pub string: bool,
    pub line: usize,
}

/// A deliberately small lexical reader: quoted examples and commented calls cannot
/// become registrations. Values stay strings; this does not execute routing code.
pub(super) fn tokens(source: &str, vb: bool) -> Vec<Token> {
    let bytes = source.as_bytes();
    let (mut i, mut line) = (0, 1);
    let mut out = Vec::new();
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            line += 1;
            i += 1;
            continue;
        }
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if (vb && bytes[i] == b'\'') || (!vb && bytes[i..].starts_with(b"//")) {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if !vb && bytes[i..].starts_with(b"/*") {
            i += 2;
            while i < bytes.len() && !bytes[i..].starts_with(b"*/") {
                if bytes[i] == b'\n' {
                    line += 1;
                }
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        let start = i;
        let start_line = line;
        if bytes[i] == b'"' || (!vb && bytes[i] == b'\'') {
            let quote = bytes[i];
            i += 1;
            let mut value = Vec::new();
            while i < bytes.len() {
                if bytes[i] == quote {
                    if vb && bytes.get(i + 1) == Some(&quote) {
                        value.push(quote);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                if !vb && bytes[i] == b'\\' {
                    // Nonliteral escapes are retained, so they cannot silently
                    // match a different URL or filesystem target.
                    value.push(bytes[i]);
                    i += 1;
                    if i == bytes.len() {
                        break;
                    }
                }
                if bytes[i] == b'\n' {
                    line += 1;
                }
                value.push(bytes[i]);
                i += 1;
            }
            out.push(Token {
                text: String::from_utf8_lossy(&value).into_owned(),
                string: true,
                line: start_line,
            });
        } else if bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' {
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let text = &source[start..i];
            if vb && text.eq_ignore_ascii_case("rem") {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            } else {
                out.push(Token {
                    text: text.to_owned(),
                    string: false,
                    line: start_line,
                });
            }
        } else {
            // Only ASCII punctuation participates in the grammar. Advance whole
            // Unicode scalars so source slicing always remains on a boundary.
            let ch = source[i..].chars().next().expect("nonempty source tail");
            i += ch.len_utf8();
            out.push(Token {
                text: ch.to_string(),
                string: false,
                line: start_line,
            });
        }
    }
    out
}

fn clean_path(raw: &str) -> anyhow::Result<String> {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            anyhow::ensure!(i + 2 < bytes.len(), "Incomplete URL escape");
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3])?;
            decoded.push(
                u8::from_str_radix(hex, 16).map_err(|_| anyhow::anyhow!("Invalid URL escape"))?,
            );
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    let path = String::from_utf8(decoded)?.replace('\\', "/");
    anyhow::ensure!(
        !path.chars().any(|c| c.is_control()) && !path.contains(':') && !path.contains('%'),
        "Invalid request path"
    );
    let path = path.trim_start_matches('~').trim_matches('/');
    anyhow::ensure!(
        !path.split('/').any(|part| matches!(part, "." | "..")),
        "Request path traversal is not allowed"
    );
    Ok(path.to_owned())
}

fn request(raw: &str) -> anyhow::Result<(Option<String>, String)> {
    anyhow::ensure!(
        !raw.trim().is_empty() && raw.len() <= 4096,
        "entry_point must contain 1..4096 bytes"
    );
    let raw = raw.trim();
    let (method, target) = match raw.split_once(' ') {
        Some((method, target))
            if ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"]
                .contains(&method.to_ascii_uppercase().as_str()) =>
        {
            (Some(method.to_ascii_uppercase()), target.trim())
        }
        _ => (None, raw),
    };
    // Do not use URL normalization here: it removes ../ before we can reject it.
    let target = if let Some((scheme, rest)) = target.split_once("://") {
        anyhow::ensure!(
            matches!(scheme, "http" | "https"),
            "Only HTTP URLs or project-relative paths are supported"
        );
        rest.find('/').map(|start| &rest[start..]).unwrap_or("")
    } else {
        target
    };
    let target = target.split(['?', '#']).next().unwrap_or("");
    Ok((method, clean_path(target)?))
}

fn parameters(pattern: &str, request: &str) -> Option<BTreeMap<String, String>> {
    let pattern: Vec<_> = pattern.trim_matches('/').split('/').collect();
    let parts: Vec<_> = request.trim_matches('/').split('/').collect();
    let mut values = BTreeMap::new();
    let mut i = 0;
    for (position, segment) in pattern.iter().enumerate() {
        if let Some(key) = segment.strip_prefix("{*").and_then(|s| s.strip_suffix('}')) {
            if key.is_empty() || position + 1 != pattern.len() {
                return None;
            }
            values.insert(key.into(), parts[i..].join("/"));
            return Some(values);
        }
        let part = *parts.get(i)?;
        if let Some(key) = segment.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            if key.is_empty() || part.is_empty() || key.contains(['{', '}', ':', '?']) {
                return None;
            }
            values.insert(key.into(), part.into());
        } else if !segment.eq_ignore_ascii_case(part) {
            return None;
        }
        i += 1;
    }
    (i == parts.len()).then_some(values)
}

pub(super) fn app_root<'a>(file: &str, sources: &'a [Source<'_>]) -> Option<&'a str> {
    sources
        .iter()
        .filter_map(|source| {
            let (parent, name) = source.path.rsplit_once('/').unwrap_or(("", source.path));
            name.eq_ignore_ascii_case("Global.asax").then_some(parent)
        })
        .filter(|parent| parent.is_empty() || file.starts_with(&format!("{parent}/")))
        .max_by_key(|parent| parent.len())
}

/// Resolve evidence candidates, retaining duplicate registrations and source locations.
/// Route ordering, constraints and registration control flow require runtime/source review.
pub fn resolve(entry: &str, sources: &[Source<'_>]) -> anyhow::Result<Routing> {
    let (method, path) = request(entry)?;
    let mut result = Routing {
        request_path: path.clone(),
        method,
        candidates: Vec::new(),
        gaps: Vec::new(),
        interpretation: "Static source candidates, not a runtime dispatch trace. Registration execution, route ordering, constraints, rewrites, authentication and HTTP-method applicability are not established.",
    };
    for source in sources {
        let lower = source.path.to_ascii_lowercase();
        if lower.ends_with(".aspx") || lower.ends_with(".ashx") || lower.ends_with(".asmx") {
            let relative = app_root(source.path, sources).map(|root| {
                source
                    .path
                    .strip_prefix(root)
                    .unwrap_or(source.path)
                    .trim_start_matches('/')
            });
            if source.path.eq_ignore_ascii_case(&path)
                || relative.is_some_and(|p| p.eq_ignore_ascii_case(&path))
            {
                result.candidates.push(RouteCandidate {
                    page: source.path.into(),
                    route_name: None,
                    pattern: path.clone(),
                    parameters: BTreeMap::new(),
                    source_path: source.path.into(),
                    source_line: 1,
                    source_blake3: blake3::hash(source.content.as_bytes()).to_hex().to_string(),
                    evidence: source
                        .content
                        .lines()
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(400)
                        .collect(),
                    basis: "physical_page_candidate",
                });
            }
        }
        if !lower.ends_with(".vb") && !lower.ends_with(".cs") {
            continue;
        }
        let tokens = tokens(source.content, lower.ends_with(".vb"));
        for window in tokens.windows(8) {
            if window[0].text != "."
                || window[1].string
                || !window[1].text.eq_ignore_ascii_case("MapPageRoute")
                || window[2].text != "("
                || !window[3].string
                || window[4].text != ","
                || !window[5].string
                || window[6].text != ","
                || !window[7].string
            {
                continue;
            }
            let Some(values) = parameters(&window[5].text, &path) else {
                continue;
            };
            let Some(root) = app_root(source.path, sources) else {
                result.gaps.push(format!(
                    "{}:{}: matching route has no established application root (Global.asax)",
                    source.path, window[1].line
                ));
                continue;
            };
            let target = &window[7].text;
            let Some(target) = target.strip_prefix("~/") else {
                result.gaps.push(format!(
                    "{}:{}: matching route target is not application-relative",
                    source.path, window[1].line
                ));
                continue;
            };
            let target = match clean_path(target) {
                Ok(target) => target,
                Err(_) => {
                    result.gaps.push(format!(
                        "{}:{}: invalid route target",
                        source.path, window[1].line
                    ));
                    continue;
                }
            };
            let page = if root.is_empty() {
                target
            } else {
                format!("{root}/{target}")
            };
            let Some(actual) = sources.iter().find(|s| s.path.eq_ignore_ascii_case(&page)) else {
                result.gaps.push(format!(
                    "{}:{}: route target {page} absent from selected source inventory",
                    source.path, window[1].line
                ));
                continue;
            };
            result.candidates.push(RouteCandidate {
                page: actual.path.into(),
                route_name: Some(window[3].text.clone()),
                pattern: window[5].text.clone(),
                parameters: values,
                source_path: source.path.into(),
                source_line: window[1].line,
                source_blake3: blake3::hash(source.content.as_bytes()).to_hex().to_string(),
                evidence: source
                    .content
                    .lines()
                    .nth(window[1].line - 1)
                    .unwrap_or("")
                    .chars()
                    .take(400)
                    .collect(),
                basis: "authored_map_page_route_candidate",
            });
        }
    }
    if result.candidates.is_empty() {
        result.gaps.push("No route/page candidate in selected sources. Dynamic routes, optional defaults, composite segments and URL rewrites are not resolved.".into());
    }
    if result.candidates.len() > 1 {
        result
            .gaps
            .push("Multiple source candidates; runtime precedence is not inferred.".into());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sources(code: &str) -> [Source<'_>; 3] {
        [
            Source {
                path: "Site/Global.asax",
                content: "<%@ Application %>",
            },
            Source {
                path: "Site/App_Code/RouteConfig.vb",
                content: code,
            },
            Source {
                path: "Site/admin/project.aspx",
                content: "<%@ Page CodeFile=\"project.aspx.vb\" %>",
            },
        ]
    }
    #[test]
    fn resolves_webforms_url_with_evidence_and_parameters() {
        let files = sources(
            "routes.MapPageRoute(\"project_edit\", \"dashboard/system/project/{pr_id}\", \"~/admin/project.aspx\")",
        );
        let result = resolve(
            "POST https://example.test/dashboard/system/project/42?tab=details",
            &files,
        )
        .unwrap();
        assert_eq!(result.method.as_deref(), Some("POST"));
        assert_eq!(result.candidates.len(), 1);
        assert_eq!(result.candidates[0].page, "Site/admin/project.aspx");
        assert_eq!(result.candidates[0].parameters["pr_id"], "42");
        assert_eq!(result.candidates[0].source_line, 1);
        assert!(result.candidates[0].evidence.contains("project_edit"));
        assert_eq!(
            resolve("/admin/project.aspx", &files).unwrap().candidates[0].basis,
            "physical_page_candidate"
        );
    }
    #[test]
    fn commented_and_string_example_routes_do_not_dispatch() {
        let files = sources(
            "' routes.MapPageRoute(\"fake\", \"fake\", \"~/admin/project.aspx\")\nREM routes.MapPageRoute(\"fake\", \"fake\", \"~/admin/project.aspx\")\nDim example = \"routes.MapPageRoute(\"\"fake\"\", \"\"fake\"\", \"\"~/admin/project.aspx\"\")\"",
        );
        assert!(resolve("/fake", &files).unwrap().candidates.is_empty());
    }
    #[test]
    fn retains_competing_routes_instead_of_guessing_precedence() {
        let files = sources(
            "routes.MapPageRoute(\"one\", \"p/{id}\", \"~/admin/project.aspx\")\nroutes.MapPageRoute(\"two\", \"p/{id}\", \"~/admin/project.aspx\")",
        );
        let result = resolve("p/3", &files).unwrap();
        assert_eq!(result.candidates.len(), 2);
        assert!(result.gaps.iter().any(|s| s.contains("Multiple")));
    }
    #[test]
    fn rejects_traversal_before_url_normalization() {
        for path in [
            "../secret.aspx",
            "/%2e%2e/secret.aspx",
            "https://host/a/../secret.aspx",
            "/%252e%252e/secret.aspx",
            "C:\\secret.aspx",
        ] {
            assert!(resolve(path, &[]).is_err(), "{path}");
        }
    }
    #[test]
    fn requires_target_and_hosting_evidence() {
        let files = sources("routes.MapPageRoute(\"one\", \"p/{id}\", \"~/missing.aspx\")");
        let result = resolve("p/3", &files).unwrap();
        assert!(result.candidates.is_empty());
        assert!(result.gaps.iter().any(|s| s.contains("absent")));
        let result = resolve("p/3", &files[1..]).unwrap();
        assert!(result.gaps.iter().any(|s| s.contains("application root")));
    }
}
