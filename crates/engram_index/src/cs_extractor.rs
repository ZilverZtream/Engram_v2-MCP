use crate::parsing::{ExtractedEdge, ExtractedSymbol, SymbolExtractor};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;

static RE_CLASS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*(?:public|private|protected|internal|static|sealed|abstract|partial|new|unsafe|\s)+\s+class\s+([A-Za-z_][A-Za-z0-9_]*)")
        .expect("valid class regex")
});
static RE_CTOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:public|private|protected|internal)\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(")
        .expect("valid ctor regex")
});
static RE_PROP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:public|private|protected|internal)\s+[A-Za-z_][A-Za-z0-9_<>,\.\?\[\]\s]*\s+([A-Za-z_][A-Za-z0-9_]*)\s*\{[^}]*\b(?:get|set|init)\b")
        .expect("valid prop regex")
});
static RE_EVENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:public|private|protected|internal)\s+event\s+[A-Za-z_][A-Za-z0-9_<>,\.\?\[\]]*\s+([A-Za-z_][A-Za-z0-9_]*)")
        .expect("valid event regex")
});
static RE_DELEGATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:public|private|protected|internal)?\s*delegate\s+[A-Za-z_][A-Za-z0-9_<>,\.\?\[\]\s]*\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(")
        .expect("valid delegate regex")
});
static RE_LOCAL_FN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:static\s+)?(?:async\s+)?[A-Za-z_][A-Za-z0-9_<>,\.\?\[\]\s]*\s+([A-Za-z_][A-Za-z0-9_]*)\s*\([^;]*\)\s*\{")
        .expect("valid local-fn regex")
});
static RE_EVENT_WIRING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"([A-Za-z_][A-Za-z0-9_\.]*)\s*\+=\s*(?:new\s+[A-Za-z_][A-Za-z0-9_<>\.,\?]*\s*\(\s*)?([A-Za-z_][A-Za-z0-9_\.]*)")
        .expect("valid event wiring regex")
});
static RE_SQL_CMD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"new\s+SqlCommand\s*\(\s*(?:@\"([^\"]*)\"|\"((?:\\.|[^\"\\])*)\")"#)
        .expect("valid sql command regex")
});
static RE_COMMAND_TEXT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"CommandText\s*=\s*(?:@\"([^\"]*)\"|\"((?:\\.|[^\"\\])*)\")"#)
        .expect("valid CommandText regex")
});
static RE_SQL_DAPPER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\.(?:Query|Execute|ExecuteScalar|QueryFirst|QuerySingle|QueryAsync|ExecuteAsync)\s*\(\s*(?:@\"([^\"]*)\"|\"((?:\\.|[^\"\\])*)\")"#)
        .expect("valid dapper regex")
});

pub fn extract_cs(path: &Path, source: &str) -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>) {
    let extractor = SymbolExtractor::new();
    let (mut symbols, mut edges) = extractor.extract(path, source);

    let method_ranges: Vec<(u32, u32, String)> = symbols
        .iter()
        .filter(|s| s.kind == "function")
        .map(|s| {
            let fqn = s
                .metadata
                .as_ref()
                .and_then(|m| m.get("fqn"))
                .cloned()
                .unwrap_or_else(|| s.name.clone());
            (s.start_line, s.end_line, fqn)
        })
        .collect();

    let mut class_name = String::new();
    for (idx, raw) in source.lines().enumerate() {
        let line_no = idx as u32 + 1;
        let line = raw.trim();

        if let Some(c) = RE_CLASS.captures(line) {
            class_name = c.get(1).map(|m| m.as_str()).unwrap_or_default().to_string();
        }

        if let Some(c) = RE_CTOR.captures(line) {
            let ctor = c.get(1).map(|m| m.as_str()).unwrap_or_default();
            if !class_name.is_empty() && ctor == class_name {
                symbols.push(ExtractedSymbol {
                    name: ctor.to_string(),
                    kind: "constructor".to_string(),
                    start_line: line_no,
                    end_line: line_no,
                    metadata: None,
                });
            }
        }

        if let Some(c) = RE_PROP.captures(line) {
            if let Some(name) = c.get(1) {
                symbols.push(ExtractedSymbol {
                    name: name.as_str().to_string(),
                    kind: "property".to_string(),
                    start_line: line_no,
                    end_line: line_no,
                    metadata: None,
                });
            }
        }

        if let Some(c) = RE_EVENT.captures(line) {
            if let Some(name) = c.get(1) {
                symbols.push(ExtractedSymbol {
                    name: name.as_str().to_string(),
                    kind: "event".to_string(),
                    start_line: line_no,
                    end_line: line_no,
                    metadata: None,
                });
            }
        }

        if let Some(c) = RE_DELEGATE.captures(line) {
            if let Some(name) = c.get(1) {
                symbols.push(ExtractedSymbol {
                    name: name.as_str().to_string(),
                    kind: "delegate".to_string(),
                    start_line: line_no,
                    end_line: line_no,
                    metadata: None,
                });
            }
        }

        if let Some(c) = RE_LOCAL_FN.captures(line)
            && let Some(name) = c.get(1)
        {
            // Heuristic: local functions are generally nested/indented and don't have access modifiers.
            if !line.contains("public ")
                && !line.contains("private ")
                && !line.contains("protected ")
                && !line.contains("internal ")
            {
                symbols.push(ExtractedSymbol {
                    name: name.as_str().to_string(),
                    kind: "local_function".to_string(),
                    start_line: line_no,
                    end_line: line_no,
                    metadata: None,
                });
            }
        }

        for cap in RE_EVENT_WIRING.captures_iter(line) {
            let lhs = cap.get(1).map(|m| m.as_str()).unwrap_or_default();
            let handler_raw = cap.get(2).map(|m| m.as_str()).unwrap_or_default();
            if lhs.is_empty() || handler_raw.is_empty() {
                continue;
            }
            let event_name = lhs.rsplit('.').next().unwrap_or(lhs);
            let handler = handler_raw.rsplit('.').next().unwrap_or(handler_raw);
            let source_name = method_ranges
                .iter()
                .find(|(start, end, _)| *start <= line_no && *end >= line_no)
                .map(|(_, _, fqn)| fqn.clone())
                .or_else(|| {
                    if !class_name.is_empty() {
                        Some(class_name.clone())
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| "file".to_string());

            let mut meta = HashMap::new();
            meta.insert("event".to_string(), event_name.to_string());
            meta.insert("wiring_syntax".to_string(), "+=".to_string());

            edges.push(ExtractedEdge {
                source_name,
                source_kind: "function".to_string(),
                source_start_line: line_no,
                source_language: "cs".to_string(),
                target_name: handler.to_string(),
                target_kind: Some("function".to_string()),
                target_start_line: None,
                kind: "event_wiring".to_string(),
                metadata: Some(meta),
            });
        }

        for sql_cap in RE_SQL_CMD
            .captures_iter(line)
            .chain(RE_COMMAND_TEXT.captures_iter(line))
            .chain(RE_SQL_DAPPER.captures_iter(line))
        {
            let sql = sql_cap
                .get(1)
                .or_else(|| sql_cap.get(2))
                .map(|m| m.as_str())
                .unwrap_or("")
                .replace("\\\"", "\"");
            if sql.trim().is_empty() {
                continue;
            }
            let (target_name, target_kind) = classify_cs_sql(&sql);
            let source_name = method_ranges
                .iter()
                .find(|(start, end, _)| *start <= line_no && *end >= line_no)
                .map(|(_, _, fqn)| fqn.clone())
                .unwrap_or_else(|| "file".to_string());
            let mut meta = HashMap::new();
            meta.insert(
                "sql_snippet".into(),
                sql.chars().take(200).collect::<String>(),
            );
            edges.push(ExtractedEdge {
                source_name,
                source_kind: "function".to_string(),
                source_start_line: line_no,
                source_language: "cs".to_string(),
                target_name,
                target_kind: Some(target_kind.to_string()),
                target_start_line: None,
                kind: "sql_calls".to_string(),
                metadata: Some(meta),
            });
        }
    }

    for s in &mut symbols {
        if s.kind == "function"
            && let Some((stage, seq)) = webforms_lifecycle_info(&s.name)
        {
            let mut meta = s.metadata.take().unwrap_or_default();
            meta.insert("lifecycle_stage".into(), stage.into());
            meta.insert("lifecycle_sequence".into(), seq.to_string());
            meta.insert("webforms_lifecycle".into(), "true".into());
            s.metadata = Some(meta);
        }
    }

    dedupe_symbols(&mut symbols);
    dedupe_edges(&mut edges);

    (symbols, edges)
}

fn dedupe_symbols(symbols: &mut Vec<ExtractedSymbol>) {
    let mut seen = HashSet::new();
    symbols.retain(|s| seen.insert((s.name.clone(), s.kind.clone(), s.start_line, s.end_line)));
}

fn dedupe_edges(edges: &mut Vec<ExtractedEdge>) {
    let mut seen = HashSet::new();
    edges.retain(|e| {
        seen.insert((
            e.kind.clone(),
            e.source_name.clone(),
            e.target_name.clone(),
            e.source_start_line,
        ))
    });
}

fn webforms_lifecycle_info(name: &str) -> Option<(&'static str, u32)> {
    match name.to_lowercase().as_str() {
        "page_preinit" => Some(("PreInit", 1)),
        "page_init" => Some(("Init", 2)),
        "page_initcomplete" => Some(("InitComplete", 3)),
        "page_preload" => Some(("PreLoad", 4)),
        "page_load" => Some(("Load", 5)),
        "page_loadcomplete" => Some(("LoadComplete", 6)),
        "page_prerender" => Some(("PreRender", 7)),
        "page_prerendercomplete" => Some(("PreRenderComplete", 8)),
        "page_savestatecomplete" => Some(("SaveStateComplete", 9)),
        "page_render" | "render" => Some(("Render", 10)),
        "page_unload" => Some(("Unload", 11)),
        "oninit" => Some(("Init", 2)),
        "onload" => Some(("Load", 5)),
        "onprerender" => Some(("PreRender", 7)),
        "onunload" => Some(("Unload", 11)),
        _ => None,
    }
}

fn classify_cs_sql(sql: &str) -> (String, &'static str) {
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        return ("sql:inline:empty".into(), "inline_sql");
    }

    let upper = trimmed.as_bytes();
    if upper.len() >= 5 {
        let starts_exec = upper[..4].eq_ignore_ascii_case(b"EXEC");
        if starts_exec {
            let rest = if upper.len() >= 8 && upper[..7].eq_ignore_ascii_case(b"EXECUTE") {
                trimmed[7..].trim_start()
            } else if upper.len() >= 5 && (upper[4] == b' ' || upper[4] == b'\t') {
                trimmed[5..].trim_start()
            } else {
                ""
            };
            if !rest.is_empty() {
                let proc_name = rest.split_whitespace().next().unwrap_or(rest);
                let clean: String = proc_name
                    .chars()
                    .filter(|&c| c != '[' && c != ']')
                    .collect();
                if !clean.is_empty() {
                    return (format!("sql:stored_proc:{clean}"), "stored_proc");
                }
            }
        }
    }

    if !trimmed.contains(char::is_whitespace) && trimmed.len() > 2 {
        (format!("sql:stored_proc:{trimmed}"), "stored_proc")
    } else {
        let h = blake3::hash(trimmed.as_bytes()).to_hex().to_string();
        (format!("sql:inline:{}", &h[..12]), "inline_sql")
    }
}
