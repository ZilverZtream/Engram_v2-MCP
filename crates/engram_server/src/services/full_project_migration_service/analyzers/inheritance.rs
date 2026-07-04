//! Extracted analyzer: inheritance.
//!
//! Part of the Phase 2 refactor that split the 13k-line
//! `full_project_migration_service.rs` into focused submodules.
//! No behaviour was changed during the move; every function lives
//! here exactly as before, just under a narrower module boundary.

#![allow(unused_imports)]

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use engram_graph::{EdgeKind, GraphStore};
use regex::Regex;

use super::super::model::*;
// Wildcard catches parent-module `pub(super) static` / `type` /
// `pub(crate) fn` helpers that were left in the grandparent during
// the Phase 2 extraction.
use super::super::super::auth_config_service::AuthConfigMap;
use super::super::super::db_strategy_service::{self, FileDataAccessProfile};
use super::super::super::dossier_service::{self, MigrationDossier};
use super::super::super::migration_order_service::{self, MigrationOrderPlan};
use super::super::super::pattern_detection_service;
use super::super::super::state_migration_service::{self, StateMigrationReport};
use super::super::*;

pub(crate) fn resolve_inheritance_chains(
    code_files: &[(&str, &str)],
    markup_files: &[FileContent],
) -> InheritanceChainReport {
    // C# keyword blacklist for method name filtering
    const CS_KEYWORDS: &[&str] = &[
        "if",
        "else",
        "for",
        "foreach",
        "while",
        "switch",
        "catch",
        "using",
        "lock",
        "return",
        "new",
        "class",
        "struct",
        "interface",
        "enum",
        "namespace",
    ];

    // 1. Build class map: class_name → (parent_class, file_path, methods[], state_writes[], base_calls[])
    // SECOND-PASS FIX: Scope methods & state_writes to each class body, not the whole file.
    let mut class_map: std::collections::HashMap<String, ClassInfo> =
        std::collections::HashMap::new();

    for (path, content) in code_files {
        let is_vb = path.to_lowercase().ends_with(".vb");

        // Collect all class starts (with their byte positions) to determine class boundaries
        let mut class_ranges: Vec<(String, String, usize)> = Vec::new(); // (name, parent, start_pos)

        if is_vb {
            for cap in VB_CLASS_INHERITS_RE.captures_iter(content) {
                let class_name = cap[1].to_string();
                let parent = cap[2].to_string();
                let start_pos = cap.get(0).map_or(0, |m| m.start());
                class_ranges.push((class_name, parent, start_pos));
            }
        } else {
            for cap in CS_CLASS_INHERITS_RE.captures_iter(content) {
                let class_name = cap[1].to_string();
                let parent = cap[2].to_string();
                let start_pos = cap.get(0).map_or(0, |m| m.start());
                class_ranges.push((class_name, parent, start_pos));
            }
        }

        // For each class, extract methods/state_writes only from its body region
        for (ci, (class_name, parent, start_pos)) in class_ranges.iter().enumerate() {
            let end_pos = class_ranges
                .get(ci + 1)
                .map(|(_, _, p)| *p)
                .unwrap_or(content.len());
            let class_body = &content[*start_pos..end_pos];

            let methods: Vec<String> = if is_vb {
                VB_METHOD_DEF_RE
                    .captures_iter(class_body)
                    .map(|c| c[1].to_string())
                    .collect()
            } else {
                CS_METHOD_DEF_RE
                    .captures_iter(class_body)
                    .filter_map(|c| {
                        let name = c[1].to_string();
                        if CS_KEYWORDS.contains(&name.as_str()) {
                            None
                        } else {
                            Some(name)
                        }
                    })
                    .collect()
            };

            let base_calls: Vec<String> = if is_vb {
                VB_CALLS_BASE_RE
                    .captures_iter(class_body)
                    .map(|c| c[1].to_string())
                    .collect()
            } else {
                CS_CALLS_BASE_RE
                    .captures_iter(class_body)
                    .map(|c| c[1].to_string())
                    .collect()
            };

            let state_writes: Vec<String> = SESSION_WRITE_RE
                .captures_iter(class_body)
                .map(|c| c[1].to_string())
                .collect();

            // THIRD-PASS FIX: Merge instead of overwrite when partial classes
            // span multiple files (e.g. _Default.aspx.vb + _Default.aspx.designer.vb).
            // The second insert would clobber the first file's methods.
            if let Some(existing) = class_map.get_mut(class_name) {
                // Keep the parent from the file that declares the inheritance
                if existing.0.is_empty() || existing.0 == class_name.as_str() {
                    existing.0 = parent.clone();
                }
                // Merge methods, state_writes, base_calls (deduplicated)
                for m in &methods {
                    if !existing.2.contains(m) {
                        existing.2.push(m.clone());
                    }
                }
                for sw in &state_writes {
                    if !existing.3.contains(sw) {
                        existing.3.push(sw.clone());
                    }
                }
                for bc in &base_calls {
                    if !existing.4.contains(bc) {
                        existing.4.push(bc.clone());
                    }
                }
            } else {
                class_map.insert(
                    class_name.clone(),
                    (
                        parent.clone(),
                        path.to_string(),
                        methods,
                        state_writes,
                        base_calls,
                    ),
                );
            }
        }
    }

    // 2. For each .aspx Inherits directive, walk the chain
    let mut chains: Vec<InheritanceChain> = Vec::new();
    let mut base_class_usage: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();

    for fc in markup_files {
        let inherits_class = INHERITS_DIRECTIVE_RE
            .captures(&fc.markup_content)
            .and_then(|c| {
                let full = c[1].to_string();
                // Extract just the class name (after last dot)
                full.rsplit('.').next().map(|s| s.to_string())
            });

        let Some(page_class) = inherits_class else {
            continue;
        };

        let mut chain: Vec<String> = Vec::new();
        let mut inherited_lifecycle: Vec<(String, String)> = Vec::new();
        let mut inherited_state_writes: Vec<String> = Vec::new();
        let mut current = page_class.clone();

        // Walk up the inheritance chain
        for _ in 0..20 {
            // max depth safety
            chain.push(current.clone());

            let Some((parent, _path, methods, state_writes, _base_calls)) = class_map.get(&current)
            else {
                // Check if parent is a known framework class
                if current == "Page"
                    || current == "System.Web.UI.Page"
                    || current == "UserControl"
                    || current == "MasterPage"
                {
                    chain.push(format!("System.Web.UI.{current}"));
                }
                break;
            };

            // Track which base classes are used
            base_class_usage
                .entry(current.clone())
                .or_default()
                .push(fc.file_path.clone());

            // Collect lifecycle methods from this ancestor
            for method in methods {
                if LIFECYCLE_METHODS
                    .iter()
                    .any(|lm| lm.eq_ignore_ascii_case(method))
                {
                    inherited_lifecycle.push((method.clone(), current.clone()));
                }
            }

            // Collect state writes from ancestors (not the page class itself)
            if current != page_class {
                for key in state_writes {
                    if !inherited_state_writes.contains(key) {
                        inherited_state_writes.push(key.clone());
                    }
                }
            }

            current = parent.clone();
        }

        if chain.len() > 1 {
            chains.push(InheritanceChain {
                page_file: fc.file_path.clone(),
                chain,
                inherited_lifecycle_methods: inherited_lifecycle,
                inherited_state_writes,
            });
        }
    }

    // 3. Build base class info
    let mut base_classes: Vec<BaseClassInfo> = Vec::new();
    for (class_name, pages) in &base_class_usage {
        if let Some((_, file_path, methods, state_writes, _)) = class_map.get(class_name) {
            let lifecycle_methods: Vec<String> = methods
                .iter()
                .filter(|m| {
                    LIFECYCLE_METHODS
                        .iter()
                        .any(|lm| lm.eq_ignore_ascii_case(m))
                })
                .cloned()
                .collect();

            if pages.len() > 1 || !lifecycle_methods.is_empty() {
                base_classes.push(BaseClassInfo {
                    class_name: class_name.clone(),
                    file_path: file_path.clone(),
                    derived_count: pages.len(),
                    lifecycle_methods,
                    state_keys_initialized: state_writes.clone(),
                });
            }
        }
    }
    base_classes.sort_by(|a, b| b.derived_count.cmp(&a.derived_count));

    // 4. Build shared lifecycle methods
    let mut shared_lifecycle: Vec<SharedLifecycleMethod> = Vec::new();
    for lm_name in LIFECYCLE_METHODS {
        let mut defining_classes: Vec<(String, Vec<String>)> = Vec::new();

        for (class_name, (_, _, methods, _, base_calls)) in &class_map {
            if methods.iter().any(|m| m.eq_ignore_ascii_case(lm_name)) {
                let calls_base = base_calls.iter().any(|bc| bc.eq_ignore_ascii_case(lm_name));
                defining_classes.push((
                    class_name.clone(),
                    if calls_base {
                        vec!["calls_base".to_string()]
                    } else {
                        vec![]
                    },
                ));
            }
        }

        if defining_classes.len() > 1 {
            let first = defining_classes[0].0.clone();
            let calls_base = !defining_classes[0].1.is_empty();
            let overridden_in: Vec<String> = defining_classes[1..]
                .iter()
                .map(|(name, _)| name.clone())
                .collect();

            shared_lifecycle.push(SharedLifecycleMethod {
                method_name: lm_name.to_string(),
                defining_class: first,
                overridden_in,
                calls_base,
            });
        }
    }

    // 5. Propagate effects down inheritance chains
    let inherited_effects = propagate_inherited_effects(&chains, code_files);

    let deepest = chains.iter().map(|c| c.chain.len()).max().unwrap_or(0);

    InheritanceChainReport {
        chains,
        base_classes,
        shared_lifecycle_methods: shared_lifecycle,
        inherited_effects,
        deepest_chain_depth: deepest,
    }
}

/// Extract effects from a method body snippet.
pub(crate) fn extract_method_effects(method_body: &str) -> Vec<String> {
    let mut effects = Vec::new();

    // Session/ViewState writes
    let session_keys: Vec<String> = SESSION_WRITE_RE
        .captures_iter(method_body)
        .map(|c| format!("Session[\"{}\"]", &c[1]))
        .collect();
    if !session_keys.is_empty() {
        effects.push(format!("State_Access: writes {}", session_keys.join(", ")));
    }

    // SQL operations
    if EFFECT_SQL_RE.is_match(method_body) {
        effects.push("SQL_Access".to_string());
    }

    // Redirects
    if EFFECT_REDIRECT_RE.is_match(method_body) {
        effects.push("Redirect".to_string());
    }

    // Control writes (UI mutation)
    if EFFECT_CONTROL_WRITE_RE.is_match(method_body) {
        effects.push("UI_Mutation".to_string());
    }

    // HTTP response manipulation
    if EFFECT_HTTP_RE.is_match(method_body) {
        effects.push("HTTP_Response".to_string());
    }

    effects
}

/// Extract method bodies from a class region of code.
pub(crate) fn extract_method_bodies_from_class(
    class_body: &str,
    is_vb: bool,
) -> Vec<(String, String)> {
    let mut results: Vec<(String, String)> = Vec::new();

    let method_re = if is_vb {
        &*VB_METHOD_DEF_RE
    } else {
        &*CS_METHOD_DEF_RE
    };

    let starts: Vec<(usize, String)> = method_re
        .captures_iter(class_body)
        .map(|c| (c.get(0).expect("match").start(), c[1].to_string()))
        .collect();

    for (i, (start, name)) in starts.iter().enumerate() {
        let end = starts
            .get(i + 1)
            .map(|(s, _)| *s)
            .unwrap_or(class_body.len());
        let body = &class_body[*start..end];
        results.push((name.clone(), body.to_string()));
    }

    results
}

/// Propagate effects from ancestor classes down to derived page classes.
pub(crate) fn propagate_inherited_effects(
    chains: &[InheritanceChain],
    code_files: &[(&str, &str)],
) -> Vec<InheritedEffect> {
    // Build class_name → (file_path, class_body) for targeted extraction
    let mut class_bodies: std::collections::HashMap<String, (bool, String)> =
        std::collections::HashMap::new();

    for (path, content) in code_files {
        let is_vb = path.to_lowercase().ends_with(".vb");
        let class_re = if is_vb {
            &*VB_CLASS_INHERITS_RE
        } else {
            &*CS_CLASS_INHERITS_RE
        };

        let mut ranges: Vec<(String, usize)> = Vec::new();
        for cap in class_re.captures_iter(content) {
            let class_name = cap[1].to_string();
            let start_pos = cap.get(0).map_or(0, |m| m.start());
            ranges.push((class_name, start_pos));
        }

        for (ci, (class_name, start_pos)) in ranges.iter().enumerate() {
            let end_pos = ranges.get(ci + 1).map(|(_, p)| *p).unwrap_or(content.len());
            let body = content[*start_pos..end_pos].to_string();
            class_bodies.insert(class_name.clone(), (is_vb, body));
        }
    }

    let mut inherited_effects: Vec<InheritedEffect> = Vec::new();

    for chain in chains {
        if chain.chain.len() < 2 {
            continue;
        }

        let page_class = &chain.chain[0];

        // Walk ancestors (skip the page class itself at index 0)
        for ancestor_name in &chain.chain[1..] {
            // Skip framework base classes
            if ancestor_name.starts_with("System.Web.UI.") {
                continue;
            }

            let Some((is_vb, class_body)) = class_bodies.get(ancestor_name) else {
                continue;
            };

            let method_bodies = extract_method_bodies_from_class(class_body, *is_vb);

            for (method_name, method_body) in &method_bodies {
                let effects = extract_method_effects(method_body);
                if effects.is_empty() {
                    continue;
                }

                inherited_effects.push(InheritedEffect {
                    class: page_class.clone(),
                    inherited_from: ancestor_name.clone(),
                    method: method_name.clone(),
                    effects: effects.clone(),
                    detail: format!(
                        "{}.{} has: {}",
                        ancestor_name,
                        method_name,
                        effects.join(", ")
                    ),
                });
            }
        }
    }

    inherited_effects
}
