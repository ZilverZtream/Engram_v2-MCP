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
// Configuration reads: ConfigurationManager.AppSettings["Key"] and friends.
// Generic name-shape detection — no application-specific helper names.
static RE_CS_APPSETTINGS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"AppSettings\s*\[\s*"([^"]+)"\s*\]"#).expect("valid appsettings regex")
});
// Settings-STORE property reads (ConfigSettings.Multitenant.IsMaster) —
// parity with the VB shape; see RE_VB_SETTINGS_STORE in vb_extractor.rs.
static RE_CS_SETTINGS_STORE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b([A-Za-z_]\w*(?:Setting|Config|UserAccess|Permission)\w*)\.((?:[A-Za-z_]\w*\.){0,2}[A-Za-z_]\w*)\b",
    )
    .expect("valid CS settings-store regex")
});
/// Permission-check calls by name shape: IsInRole / IsUserInRole and the
/// custom-helper families legacy apps grow (IsXxxAdmin, CheckAccessLevel,
/// HasPermission, RequireRole, DemandAdmin, Authorize...). Matching is on
/// the call-site name only — works for any project's helpers.
static RE_GUARD_CALL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(is[a-z0-9_]*admin[a-z0-9_]*|isinrole|isuserinrole|is[a-z0-9_]*role|check[a-z0-9_]*(access|permission|role)[a-z0-9_]*|has[a-z0-9_]*(permission|access|role)[a-z0-9_]*|require[a-z0-9_]*(role|permission|admin)[a-z0-9_]*|demand[a-z0-9_]*|authorize[a-z0-9_]*)\s*\(",
    )
    .expect("valid guard regex")
});
/// Role string literal passed to a role check: IsInRole("Admin").
static RE_GUARD_ROLE_LITERAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\b(?:isinrole|isuserinrole)\s*\(\s*"([^"]+)""#)
        .expect("valid role literal regex")
});
/// Base list on a class declaration: `class Orders : PageBase, IAuditable {`.
static RE_CLASS_BASES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bclass\s+[A-Za-z_][A-Za-z0-9_]*(?:\s*<[^>{:]*>)?\s*:\s*([^{]+)")
        .expect("valid base list regex")
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
    let mut guard_hits: Vec<(u32, String)> = Vec::new();
    let mut role_hits: Vec<(u32, String)> = Vec::new();
    for (idx, raw) in source.lines().enumerate() {
        let line_no = idx as u32 + 1;
        let line = raw.trim();

        if let Some(c) = RE_CLASS.captures(line) {
            class_name = c.get(1).map(|m| m.as_str()).unwrap_or_default().to_string();

            // Type hierarchy: base classes and interfaces from the base list.
            // Interface heuristic is the .NET naming convention (leading
            // 'I' + uppercase); everything else is treated as the base class.
            if let Some(bases) = RE_CLASS_BASES
                .captures(line)
                .and_then(|b| b.get(1).map(|m| m.as_str()))
            {
                for raw_base in bases.split(',') {
                    // Strip generic args + whitespace; keep dotted names.
                    let base = raw_base
                        .split(['<', '('])
                        .next()
                        .unwrap_or(raw_base)
                        .trim()
                        .trim_end_matches('{')
                        .trim();
                    if base.is_empty() || class_name.is_empty() {
                        continue;
                    }
                    let terminal = base.rsplit('.').next().unwrap_or(base);
                    let is_interface = terminal.len() >= 2
                        && terminal.starts_with('I')
                        && terminal
                            .chars()
                            .nth(1)
                            .is_some_and(|c| c.is_ascii_uppercase());
                    edges.push(ExtractedEdge {
                        source_name: class_name.clone(),
                        source_kind: "class".to_string(),
                        source_start_line: line_no,
                        source_language: "cs".to_string(),
                        target_name: base.to_string(),
                        target_kind: Some("class".to_string()),
                        target_start_line: None,
                        kind: if is_interface {
                            "implements_interface".to_string()
                        } else {
                            "inherits_from".to_string()
                        },
                        metadata: None,
                    });
                }
            }
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

        // ── Settings reads ──────────────────────────────────────────────
        for cap in RE_CS_APPSETTINGS.captures_iter(line) {
            let Some(key) = cap.get(1).map(|m| m.as_str()) else {
                continue;
            };
            let source_name = method_ranges
                .iter()
                .find(|(start, end, _)| *start <= line_no && *end >= line_no)
                .map(|(_, _, fqn)| fqn.clone())
                .unwrap_or_else(|| "file".to_string());
            edges.push(ExtractedEdge {
                source_name,
                source_kind: "function".to_string(),
                source_start_line: line_no,
                source_language: "cs".to_string(),
                target_name: key.to_string(),
                // app_setting symbols come from web.config extraction; the
                // ingest batch resolver (or ::placeholder + post-resolver)
                // links this read to the real setting node.
                target_kind: Some("app_setting".to_string()),
                target_start_line: None,
                kind: "reads_setting".to_string(),
                metadata: None,
            });
        }

        // ── Settings-store property reads (ConfigSettings.X.Y) ─────────
        for cap in RE_CS_SETTINGS_STORE.captures_iter(line) {
            let (Some(root), Some(tail)) = (cap.get(1), cap.get(2)) else {
                continue;
            };
            let root_l = root.as_str().to_lowercase();
            if matches!(
                root_l.as_str(),
                "configurationmanager" | "webconfigurationmanager" | "configurationsettings"
            ) {
                continue;
            }
            // Method calls are not settings reads.
            let after = line[tail.end()..].trim_start();
            if after.starts_with('(') {
                continue;
            }
            let tail_l = tail.as_str().to_lowercase();
            if tail_l == "appsettings" || tail_l.starts_with("appsettings") {
                continue;
            }
            let source_name = method_ranges
                .iter()
                .find(|(start, end, _)| *start <= line_no && *end >= line_no)
                .map(|(_, _, fqn)| fqn.clone())
                .unwrap_or_else(|| "file".to_string());
            edges.push(ExtractedEdge {
                source_name,
                source_kind: "function".to_string(),
                source_start_line: line_no,
                source_language: "cs".to_string(),
                target_name: format!("{}.{}", root.as_str(), tail.as_str()),
                target_kind: Some("app_setting".to_string()),
                target_start_line: None,
                kind: "reads_setting".to_string(),
                metadata: None,
            });
        }

        // ── Permission checks (annotated onto the enclosing method) ────
        for cap in RE_GUARD_CALL.captures_iter(line) {
            if let Some(name) = cap.get(1) {
                let guard = name.as_str().to_string();
                guard_hits.push((line_no, guard));
            }
        }
        for cap in RE_GUARD_ROLE_LITERAL.captures_iter(line) {
            if let Some(role) = cap.get(1) {
                role_hits.push((line_no, role.as_str().to_string()));
            }
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

    // Attach guard facts to the enclosing function symbols so the graph can
    // answer "which permission checks protect this method".
    annotate_guards(&mut symbols, &guard_hits, &role_hits);

    dedupe_symbols(&mut symbols);
    dedupe_edges(&mut edges);

    (symbols, edges)
}

/// Attach `permission_checks` / `guard_roles` metadata to the function
/// symbols whose line range contains each detected guard call.
pub(crate) fn annotate_guards(
    symbols: &mut [ExtractedSymbol],
    guard_hits: &[(u32, String)],
    role_hits: &[(u32, String)],
) {
    if guard_hits.is_empty() && role_hits.is_empty() {
        return;
    }
    for s in symbols.iter_mut() {
        if s.kind != "function" {
            continue;
        }
        let mut guards: Vec<String> = guard_hits
            .iter()
            .filter(|(l, _)| *l >= s.start_line && *l <= s.end_line)
            .map(|(_, g)| g.to_lowercase())
            .collect();
        guards.sort();
        guards.dedup();
        let mut roles: Vec<String> = role_hits
            .iter()
            .filter(|(l, _)| *l >= s.start_line && *l <= s.end_line)
            .map(|(_, r)| r.clone())
            .collect();
        roles.sort();
        roles.dedup();
        if guards.is_empty() && roles.is_empty() {
            continue;
        }
        let mut meta = s.metadata.take().unwrap_or_default();
        if !guards.is_empty() {
            meta.insert("permission_checks".into(), guards.join(";"));
        }
        if !roles.is_empty() {
            meta.insert("guard_roles".into(), roles.join(";"));
        }
        s.metadata = Some(meta);
    }
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

pub(crate) fn classify_cs_sql(sql: &str) -> (String, &'static str) {
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

#[cfg(test)]
mod guard_settings_tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn cs_settings_store_reads_emit_reads_setting_edges() {
        let code = "namespace App {\n  class P {\n    void Load() {\n      if (ConfigSettings.Multitenant.IsMaster) { }\n      var x = SystemSettingStore.General.RoqEnable;\n      var y = SettingsHelper.Load(\"skip\");\n    }\n  }\n}";
        let (_, edges) = super::extract_cs(Path::new("p.aspx.cs"), code);
        let settings: Vec<&str> = edges
            .iter()
            .filter(|e| e.kind == "reads_setting")
            .map(|e| e.target_name.as_str())
            .collect();
        assert!(
            settings.contains(&"ConfigSettings.Multitenant.IsMaster"),
            "{settings:?}"
        );
        assert!(
            settings
                .iter()
                .any(|s| s.starts_with("SystemSettingStore.General")),
            "{settings:?}"
        );
        assert!(
            !settings.iter().any(|s| s.ends_with(".Load")),
            "method calls excluded: {settings:?}"
        );
    }

    #[test]
    fn cs_appsettings_read_emits_reads_setting_edge() {
        let code = r#"
namespace App {
    public class UserService {
        public void AddUser() {
            var max = ConfigurationManager.AppSettings["MaxUserCount"];
        }
    }
}"#;
        let (_, edges) = extract_cs(Path::new("UserService.cs"), code);
        let setting = edges
            .iter()
            .find(|e| e.kind == "reads_setting")
            .expect("reads_setting edge expected");
        assert_eq!(setting.target_name, "MaxUserCount");
        assert_eq!(setting.target_kind.as_deref(), Some("app_setting"));
        assert!(
            setting.source_name.contains("AddUser"),
            "edge source should be the enclosing method, got {}",
            setting.source_name
        );
    }

    #[test]
    fn cs_guard_calls_annotate_enclosing_function() {
        let code = r#"
namespace App {
    public class AdminApi {
        public void AddUser() {
            if (!User.IsInRole("Admin")) { return; }
            if (!CheckAccessLevelByAccessObject(7)) { return; }
        }
        public void ListUsers() { }
    }
}"#;
        let (symbols, _) = extract_cs(Path::new("AdminApi.cs"), code);
        let add_user = symbols
            .iter()
            .find(|s| s.kind == "function" && s.name == "AddUser")
            .expect("AddUser symbol");
        let meta = add_user.metadata.as_ref().expect("guard metadata");
        let checks = meta.get("permission_checks").expect("permission_checks");
        assert!(checks.contains("isinrole"), "got {checks}");
        assert!(
            checks.contains("checkaccesslevelbyaccessobject"),
            "custom guard helper must be caught by name shape, got {checks}"
        );
        assert_eq!(meta.get("guard_roles").map(String::as_str), Some("Admin"));

        let list_users = symbols
            .iter()
            .find(|s| s.kind == "function" && s.name == "ListUsers")
            .expect("ListUsers symbol");
        let unguarded = list_users
            .metadata
            .as_ref()
            .and_then(|m| m.get("permission_checks"));
        assert!(unguarded.is_none(), "ListUsers has no guards");
    }
}

#[cfg(test)]
mod hierarchy_tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn cs_base_list_splits_class_vs_interfaces() {
        let code = r#"
namespace App {
    public class OrdersPage : PageBase, IAuditable {
        public void Load() {}
    }
}"#;
        let (_, edges) = extract_cs(Path::new("OrdersPage.aspx.cs"), code);
        let inherits: Vec<&str> = edges
            .iter()
            .filter(|e| e.kind == "inherits_from")
            .map(|e| e.target_name.as_str())
            .collect();
        assert_eq!(inherits, vec!["PageBase"]);
        let implements: Vec<&str> = edges
            .iter()
            .filter(|e| e.kind == "implements_interface")
            .map(|e| e.target_name.as_str())
            .collect();
        assert_eq!(implements, vec!["IAuditable"]);
    }

    #[test]
    fn cs_generic_base_and_qualified_interface() {
        let code = "public class Repo : BaseRepo<Order>, System.IDisposable { }";
        let (_, edges) = extract_cs(Path::new("Repo.cs"), code);
        let inherits: Vec<&str> = edges
            .iter()
            .filter(|e| e.kind == "inherits_from")
            .map(|e| e.target_name.as_str())
            .collect();
        assert_eq!(inherits, vec!["BaseRepo"], "generic args stripped");
        let implements: Vec<&str> = edges
            .iter()
            .filter(|e| e.kind == "implements_interface")
            .map(|e| e.target_name.as_str())
            .collect();
        assert_eq!(
            implements,
            vec!["System.IDisposable"],
            "interface detection uses the terminal segment"
        );
    }
}
