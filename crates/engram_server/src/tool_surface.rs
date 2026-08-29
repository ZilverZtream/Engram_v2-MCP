//! Advertised tool-surface hygiene.
//!
//! Strict function-schema validators (OpenAI / GitHub Copilot) reject a
//! `tools/list` whose schemas use `$ref`/`definitions`/`$defs` or an `anyOf`
//! whose only job is `T | null`. `schemars` emits exactly those for nested
//! request structs (for example `ask_codebase`'s `as_of`/`audience`) and every
//! `Option<Struct>` field. When such a schema reaches the model API the whole
//! request fails with HTTP 400 — the moment the server is activated, before any
//! tool even runs. `sanitize_schema` rewrites each advertised schema into a
//! self-contained draft-07 object schema: refs inlined, `T | null` collapsed to
//! a nullable `T`, definition tables dropped. Tool *behavior* is untouched — the
//! request structs still deserialize the same JSON; only the advertised shape
//! changes.

use serde_json::{Map, Value};

const MAX_DEPTH: usize = 16;

/// Rewrite one tool input schema into strict-validator-friendly form.
pub fn sanitize_schema(schema: &Map<String, Value>) -> Map<String, Value> {
    // Gather the definition table under either draft-07 or 2020-12 spelling.
    let mut defs: Map<String, Value> = Map::new();
    for key in ["definitions", "$defs"] {
        if let Some(Value::Object(m)) = schema.get(key) {
            for (k, v) in m {
                defs.insert(k.clone(), v.clone());
            }
        }
    }

    let mut root = schema.clone();
    root.remove("definitions");
    root.remove("$defs");
    // `$schema` is advisory and some validators dislike an unexpected draft URI
    // on a function parameter object; drop it.
    root.remove("$schema");

    let mut v = Value::Object(root);
    inline(&mut v, &defs, 0);
    match v {
        Value::Object(m) => m,
        // A schema that reduces to a non-object is degenerate; fall back to a
        // permissive object rather than emit something invalid.
        _ => {
            let mut m = Map::new();
            m.insert("type".into(), Value::String("object".into()));
            m
        }
    }
}

fn resolve<'a>(reference: &str, defs: &'a Map<String, Value>) -> Option<&'a Value> {
    // "#/definitions/Name" or "#/$defs/Name" -> "Name"
    reference.rsplit('/').next().and_then(|name| defs.get(name))
}

fn is_null_branch(v: &Value) -> bool {
    let Value::Object(m) = v else { return false };
    match m.get("type") {
        Some(Value::String(t)) if t == "null" => return true,
        Some(Value::Array(ts)) if ts.len() == 1 && ts[0] == Value::String("null".into()) => {
            return true;
        }
        _ => {}
    }
    if m.get("const") == Some(&Value::Null) {
        return true;
    }
    if let Some(Value::Array(e)) = m.get("enum") {
        if e.len() == 1 && e[0] == Value::Null {
            return true;
        }
    }
    false
}

fn inline(v: &mut Value, defs: &Map<String, Value>, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    match v {
        Value::Object(map) => {
            // 1. Bare `$ref` -> replace this node with the (inlined) target.
            if let Some(Value::String(reference)) = map.get("$ref").cloned() {
                if let Some(target) = resolve(&reference, defs) {
                    let mut resolved = target.clone();
                    inline(&mut resolved, defs, depth + 1);
                    // Carry a sibling description/default that sat next to $ref.
                    if let Value::Object(rmap) = &mut resolved {
                        for k in ["description", "default", "title"] {
                            if !rmap.contains_key(k) {
                                if let Some(val) = map.get(k) {
                                    rmap.insert(k.into(), val.clone());
                                }
                            }
                        }
                    }
                    *v = resolved;
                    return;
                }
                // Unresolvable ref -> permissive object, never a dangling $ref.
                map.remove("$ref");
                map.insert("type".into(), Value::String("object".into()));
                return;
            }

            // 2. Collapse `anyOf`/`oneOf` of the form [schema, null] -> schema.
            for combiner in ["anyOf", "oneOf"] {
                let Some(Value::Array(arr)) = map.get(combiner).cloned() else {
                    continue;
                };
                let non_null: Vec<Value> =
                    arr.iter().filter(|m| !is_null_branch(m)).cloned().collect();
                let had_null = non_null.len() != arr.len();
                if non_null.len() == 1 {
                    let mut branch = non_null.into_iter().next().unwrap();
                    inline(&mut branch, defs, depth + 1);
                    if let Value::Object(bmap) = &mut branch {
                        for k in ["description", "default", "title"] {
                            if !bmap.contains_key(k) {
                                if let Some(val) = map.get(k) {
                                    bmap.insert(k.into(), val.clone());
                                }
                            }
                        }
                        if had_null {
                            bmap.insert("nullable".into(), Value::Bool(true));
                        }
                    }
                    *v = branch;
                    return;
                }
                // Multiple real branches: keep the combiner but inline members
                // (handled by the generic recursion below).
            }

            // 3. Recurse into children.
            for (_k, child) in map.iter_mut() {
                inline(child, defs, depth + 1);
            }
        }
        Value::Array(arr) => {
            for child in arr.iter_mut() {
                inline(child, defs, depth + 1);
            }
        }
        _ => {}
    }
}

/// True when a tool should be hidden from the advertised list when the curated
/// surface is opted into (`advertise_all_tools = false`). `[.NET legacy]` tools
/// are migration-only and niche off that stack; hiding them shrinks the surface
/// for a client with a hard tool-count ceiling. Hidden tools stay fully callable
/// — this governs discovery only.
pub fn is_curated_out(description: Option<&str>) -> bool {
    description
        .map(|d| d.contains("[.NET legacy]"))
        .unwrap_or(false)
}

/// External audit 2026-08-29 (auditor P0 #6, row 0; owner decision 09:32):
/// the tools behind the ten vital capabilities plus the index/health/search
/// essentials. Advertised by default; every other tool stays CALLABLE and is
/// listed by `list_advanced_tools` (or `advertise_all_tools = true`).
pub const CORE_TOOLS: &[&str] = &[
    // 6 natural-language understanding + identity
    "ask_codebase",
    "resolve_id",
    // 1 story-to-change scope
    "plan_user_story",
    "get_change_set",
    // 4 exact entity / consumer discovery
    "get_concept_footprint",
    "find_symbol_references",
    // 2 follow the code before editing
    "get_method_edit_context",
    "get_page_context",
    // 10 change exposure and edit risk
    "check_edit_safety",
    "compute_blast_radius",
    "impact_analysis",
    // 3 pre-commit defect prevention
    "pre_commit_review",
    "pre_push_audit",
    // 8 security, settings, durable laws
    "map_guards_and_settings",
    "immune_check",
    // 5 house implementation pattern + style
    "find_implementation_pattern",
    "analyze_file_coding_style",
    // 7 causal UI / data tracing
    "trace_ui_event",
    "trace_data_flow",
    "find_connection_path",
    // 9 "you forgot the other side"
    "detect_incomplete_changes",
    "find_similar_changes",
    "begin_edit_session",
    "complete_edit_session",
    // essentials: index lifecycle, health, raw search, integration, discovery
    "index_project",
    "update_project",
    "project_health",
    "get_index_freshness",
    "grep_project",
    "search_memory",
    "produce_claude_md",
    "list_advanced_tools",
];

/// The one filter `list_tools` applies. `advertise_all = false` (the default)
/// advertises exactly the core tier; `true` advertises everything except the
/// `[.NET legacy]`-curated tools' legacy marker rule, which still applies.
pub fn advertised(items: Vec<rmcp::model::Tool>, advertise_all: bool) -> Vec<rmcp::model::Tool> {
    items
        .into_iter()
        .filter(|t| {
            if advertise_all {
                true
            } else {
                CORE_TOOLS.contains(&t.name.as_ref())
            }
        })
        .collect()
}

/// Tools that are callable but not advertised by default.
pub fn advanced(items: Vec<rmcp::model::Tool>) -> Vec<rmcp::model::Tool> {
    items
        .into_iter()
        .filter(|t| !CORE_TOOLS.contains(&t.name.as_ref()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn has_ref(v: &Value) -> bool {
        match v {
            Value::Object(m) => {
                if m.contains_key("$ref")
                    || m.contains_key("definitions")
                    || m.contains_key("$defs")
                {
                    return true;
                }
                m.values().any(has_ref)
            }
            Value::Array(a) => a.iter().any(has_ref),
            _ => false,
        }
    }

    #[test]
    fn inlines_refs_and_collapses_nullable_anyof() {
        // Mirrors ask_codebase's real shape: a nested struct behind an
        // `Option`, emitted by schemars as anyOf[$ref, {const:null}] + defs.
        let schema = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "additionalProperties": false,
            "definitions": {
                "AsOf": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "branch": {"type": "string", "nullable": true, "default": null}
                    }
                }
            },
            "properties": {
                "question": {"type": "string"},
                "as_of": {
                    "anyOf": [
                        {"$ref": "#/definitions/AsOf"},
                        {"const": null, "nullable": true}
                    ],
                    "description": "Pin the answer to a branch/commit."
                }
            },
            "required": ["question"]
        });
        let obj = schema.as_object().unwrap();
        let cleaned = sanitize_schema(obj);
        let cleaned_v = Value::Object(cleaned.clone());

        assert!(
            !has_ref(&cleaned_v),
            "no $ref/definitions may survive: {cleaned_v}"
        );
        // as_of collapsed to the inlined AsOf object, description preserved, nullable.
        let as_of = &cleaned["properties"]["as_of"];
        assert_eq!(as_of["type"], json!("object"));
        assert_eq!(as_of["nullable"], json!(true));
        assert_eq!(
            as_of["description"],
            json!("Pin the answer to a branch/commit.")
        );
        assert!(as_of["properties"].get("branch").is_some());
        // The flat parts are untouched.
        assert_eq!(cleaned["properties"]["question"]["type"], json!("string"));
        assert_eq!(cleaned["required"], json!(["question"]));
    }

    #[test]
    fn flat_schema_is_essentially_unchanged() {
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "project_id": {"type": "string"},
                "limit": {"type": "integer", "format": "uint32", "nullable": true}
            },
            "required": ["project_id"]
        });
        let cleaned = sanitize_schema(schema.as_object().unwrap());
        assert_eq!(cleaned["properties"]["project_id"]["type"], json!("string"));
        assert_eq!(cleaned["properties"]["limit"]["format"], json!("uint32"));
        assert_eq!(cleaned["required"], json!(["project_id"]));
    }

    #[test]
    fn curated_out_flags_only_legacy() {
        assert!(is_curated_out(Some("[.NET legacy] migrate the thing")));
        assert!(!is_curated_out(Some("search the codebase")));
        assert!(!is_curated_out(None));
    }
}
