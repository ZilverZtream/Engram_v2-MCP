//! Extracted analyzer: vb translation.
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
use super::super::*;
use super::super::super::auth_config_service::AuthConfigMap;
use super::super::super::db_strategy_service::{self, FileDataAccessProfile};
use super::super::super::dossier_service::{self, MigrationDossier};
use super::super::super::migration_order_service::{self, MigrationOrderPlan};
use super::super::super::pattern_detection_service;
use super::super::super::state_migration_service::{self, StateMigrationReport};


pub(crate) fn analyze_vb_translation_flags(code_files: &[(&str, &str)]) -> VbTranslationReport {
    static OPTIONAL_PARAM_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)\bOptional\s+(?:ByVal\s+|ByRef\s+)?\w+\s+As\s+").expect("valid regex")
    });
    static IS_MISSING_RE: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"(?i)\bIsMissing\s*\(").expect("valid regex"));
    static MODULE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?im)^\s*(?:Public\s+|Friend\s+)?Module\s+(\w+)").expect("valid regex")
    });
    static MY_NAMESPACE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)\bMy\.(Computer|Application|Settings|Resources|User|Forms|WebServices)\b")
            .expect("valid regex")
    });
    static WITH_EVENTS_RE: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"(?i)\bWithEvents\s+").expect("valid regex"));
    static HANDLES_RE: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"(?i)\bHandles\s+\w+\.\w+").expect("valid regex"));
    static RAISE_EVENT_RE: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"(?i)\bRaiseEvent\s+\w+").expect("valid regex"));
    static SHADOWS_RE: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"(?i)\bShadows\s+").expect("valid regex"));
    static OPTION_COMPARE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?im)^\s*Option\s+Compare\s+Text").expect("valid regex")
    });
    static LIKE_OPERATOR_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)\bLike\s+"[^"]*[*?#\[\]]+[^"]*""#).expect("valid regex")
    });
    static VB_INTRINSICS_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)\b(?:IsNumeric|IsDate|IsNothing|IsDBNull|IsArray|IsError)\s*\(")
            .expect("valid regex")
    });
    static ON_ERROR_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)\bOn\s+Error\s+(?:Resume\s+Next|GoTo\s+)").expect("valid regex")
    });
    static LATE_BINDING_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)\bDim\s+\w+\s+As\s+Object\b").expect("valid regex")
    });
    static VB_CAST_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)\b(?:CType|DirectCast|TryCast|CStr|CInt|CDbl|CBool|CLng|CDec|CDate|CByte|CShort|CSng|CObj|CChar)\s*\(").expect("valid regex")
    });

    struct FlagDef {
        category: &'static str,
        pattern_name: &'static str,
        re: &'static std::sync::LazyLock<Regex>,
        csharp_eq: &'static str,
        risk: &'static str,
        auto_tr: bool,
        notes: &'static str,
    }

    let flag_defs: Vec<FlagDef> = vec![
        FlagDef {
            category: "ErrorHandling",
            pattern_name: "On Error Resume Next / GoTo",
            re: &ON_ERROR_RE,
            csharp_eq: "try-catch blocks",
            risk: "high",
            auto_tr: false,
            notes: "Each On Error must be manually restructured into proper exception handling",
        },
        FlagDef {
            category: "OptionalParams",
            pattern_name: "Optional parameter",
            re: &OPTIONAL_PARAM_RE,
            csharp_eq: "default parameter values",
            risk: "low",
            auto_tr: true,
            notes: "",
        },
        FlagDef {
            category: "OptionalParams",
            pattern_name: "IsMissing()",
            re: &IS_MISSING_RE,
            csharp_eq: "No equivalent — restructure logic",
            risk: "high",
            auto_tr: false,
            notes: "IsMissing has no C# equivalent; refactor to nullable parameters",
        },
        FlagDef {
            category: "Modules",
            pattern_name: "Module declaration",
            re: &MODULE_RE,
            csharp_eq: "static class",
            risk: "low",
            auto_tr: true,
            notes: "",
        },
        FlagDef {
            category: "MyNamespace",
            pattern_name: "My. namespace",
            re: &MY_NAMESPACE_RE,
            csharp_eq: "System.IO / IConfiguration / etc.",
            risk: "medium",
            auto_tr: false,
            notes: "My.Settings → IConfiguration; My.Computer → System.IO",
        },
        FlagDef {
            category: "Events",
            pattern_name: "WithEvents",
            re: &WITH_EVENTS_RE,
            csharp_eq: "Explicit += / -= subscription",
            risk: "medium",
            auto_tr: false,
            notes: "",
        },
        FlagDef {
            category: "Events",
            pattern_name: "Handles clause",
            re: &HANDLES_RE,
            csharp_eq: "btn.Click += handler in constructor",
            risk: "medium",
            auto_tr: false,
            notes: "",
        },
        FlagDef {
            category: "Events",
            pattern_name: "RaiseEvent",
            re: &RAISE_EVENT_RE,
            csharp_eq: "MyEvent?.Invoke(args)",
            risk: "low",
            auto_tr: true,
            notes: "",
        },
        FlagDef {
            category: "Inheritance",
            pattern_name: "Shadows keyword",
            re: &SHADOWS_RE,
            csharp_eq: "new modifier",
            risk: "low",
            auto_tr: true,
            notes: "",
        },
        FlagDef {
            category: "StringCompare",
            pattern_name: "Option Compare Text",
            re: &OPTION_COMPARE_RE,
            csharp_eq: "StringComparer.OrdinalIgnoreCase everywhere",
            risk: "high",
            auto_tr: false,
            notes: "All string comparisons in this file use case-insensitive by default",
        },
        FlagDef {
            category: "PatternMatch",
            pattern_name: "Like operator",
            re: &LIKE_OPERATOR_RE,
            csharp_eq: "Regex.IsMatch()",
            risk: "medium",
            auto_tr: false,
            notes: "",
        },
        FlagDef {
            category: "Intrinsics",
            pattern_name: "VB intrinsics (IsNumeric, etc.)",
            re: &VB_INTRINSICS_RE,
            csharp_eq: "TryParse methods",
            risk: "low",
            auto_tr: true,
            notes: "",
        },
        FlagDef {
            category: "LateBind",
            pattern_name: "Dim x As Object (late binding)",
            re: &LATE_BINDING_RE,
            csharp_eq: "dynamic keyword",
            risk: "high",
            auto_tr: false,
            notes: "",
        },
        FlagDef {
            category: "Casting",
            pattern_name: "VB cast operators",
            re: &VB_CAST_RE,
            csharp_eq: "(Type)x or x as Type",
            risk: "low",
            auto_tr: true,
            notes: "",
        },
    ];

    let mut vb_files = 0usize;
    let mut cs_files = 0usize;
    let mut translation_flags: Vec<VbTranslationFlag> = Vec::new();
    let mut file_flag_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut option_strict_on_files = 0usize;
    let mut option_strict_off_files = 0usize;
    let mut methods_with_dynamic_dispatch = 0usize;
    let mut late_binding_call_count = 0usize;
    let mut object_var_count = 0usize;
    let mut callbyname_count = 0usize;

    // MIG1/D2: log regex compile failures so silent zero-out is visible to operators.
    macro_rules! compile_re {
        ($pattern:expr, $name:expr) => {{
            match Regex::new($pattern) {
                Ok(r) => Some(r),
                Err(e) => {
                    tracing::warn!(
                        pattern_name = $name,
                        error = %e,
                        "MIG1: VB analysis regex failed to compile — affected metric will be zero"
                    );
                    None
                }
            }
        }};
    }
    let option_strict_re =
        compile_re!(r"(?im)^\s*Option\s+Strict\s+(On|Off)\b", "option_strict_re");
    let method_re = compile_re!(
        r"(?is)\b(?:Public|Private|Protected|Friend|Shared|Overrides|Overridable|Async|Partial|MustOverride|NotOverridable|Default|Iterator|ReadOnly|WriteOnly\s+)*\b(?:Sub|Function)\b.*?\bEnd\s+(?:Sub|Function)\b",
        "method_re"
    );
    let object_decl_re = compile_re!(r"(?i)\bDim\s+(\w+)\s+As\s+Object\b", "object_decl_re");
    let callbyname_re = compile_re!(r"(?i)\bCallByName\s*\(", "callbyname_re");
    let late_call_re = compile_re!(r"(?i)\b(\w+)\.(\w+)\s*(?:\(([^)]*)\))?", "late_call_re");

    for (path, content) in code_files {
        let is_vb = path.to_lowercase().ends_with(".vb");
        let is_cs = path.to_lowercase().ends_with(".cs");
        if is_vb {
            vb_files += 1;
        } else if is_cs {
            cs_files += 1;
        }

        // Only scan VB files for VB-specific constructs
        if !is_vb {
            continue;
        }

        if let Some(re) = &option_strict_re {
            let mut file_option = None;
            for cap in re.captures_iter(content) {
                file_option = cap.get(1).map(|m| m.as_str().to_ascii_lowercase());
            }
            match file_option.as_deref() {
                Some("on") => option_strict_on_files += 1,
                Some("off") => option_strict_off_files += 1,
                _ => {}
            }
        }

        if let (Some(mre), Some(obj_re), Some(cbn_re), Some(call_re)) =
            (&method_re, &object_decl_re, &callbyname_re, &late_call_re)
        {
            for method_match in mre.find_iter(content) {
                let body = method_match.as_str();
                let mut method_object_vars = std::collections::HashSet::new();
                let mut method_object_var_count = 0usize;

                for cap in obj_re.captures_iter(body) {
                    if let Some(v) = cap.get(1).map(|m| m.as_str()) {
                        method_object_var_count += 1;
                        method_object_vars.insert(v.to_lowercase());
                    }
                }

                let method_callbyname = cbn_re.find_iter(body).count();
                let method_late_calls = call_re
                    .captures_iter(body)
                    .filter(|cap| {
                        cap.get(1)
                            .map(|m| method_object_vars.contains(&m.as_str().to_lowercase()))
                            .unwrap_or(false)
                    })
                    .count();

                if method_object_var_count > 0 || method_callbyname > 0 || method_late_calls > 0 {
                    methods_with_dynamic_dispatch += 1;
                }
                object_var_count += method_object_var_count;
                callbyname_count += method_callbyname;
                late_binding_call_count += method_late_calls;
            }
        }

        for def in &flag_defs {
            let count = def.re.find_iter(content).count();
            if count > 0 {
                translation_flags.push(VbTranslationFlag {
                    category: def.category.to_string(),
                    pattern: def.pattern_name.to_string(),
                    file_path: path.to_string(),
                    count,
                    csharp_equivalent: def.csharp_eq.to_string(),
                    risk_level: def.risk.to_string(),
                    auto_translatable: def.auto_tr,
                    notes: def.notes.to_string(),
                });
                *file_flag_counts.entry(path.to_string()).or_insert(0) += count;
            }
        }
    }

    let mut flags_by_category: BTreeMap<String, usize> = BTreeMap::new();
    for flag in &translation_flags {
        *flags_by_category.entry(flag.category.clone()).or_insert(0) += flag.count;
    }

    let total_flags: usize = translation_flags.iter().map(|f| f.count).sum();

    let mut highest_risk: Vec<(String, usize)> = file_flag_counts.into_iter().collect();
    highest_risk.sort_by(|a, b| b.1.cmp(&a.1));
    highest_risk.truncate(10);

    let dynamic_dispatch_risk_tier =
        if option_strict_off_files > 0 || callbyname_count > 0 || late_binding_call_count >= 5 {
            "high"
        } else if methods_with_dynamic_dispatch > 0 || object_var_count > 0 {
            "medium"
        } else {
            "low"
        };

    VbTranslationReport {
        is_vb_project: vb_files > cs_files,
        vb_file_count: vb_files,
        cs_file_count: cs_files,
        mixed_language: vb_files > 0 && cs_files > 0,
        total_flags,
        translation_flags,
        flags_by_category,
        highest_risk_files: highest_risk,
        dynamic_dispatch: DynamicDispatchSummary {
            option_strict_on_files,
            option_strict_off_files,
            methods_with_dynamic_dispatch,
            late_binding_call_count,
            object_var_count,
            callbyname_count,
            dynamic_dispatch_risk_tier: dynamic_dispatch_risk_tier.to_string(),
        },
    }
}

/// True iff a `VbTranslationFlag` extracted from file `flag_path` belongs
/// in the dossier for a page at `page_path` with optional detected
/// `codebehind`. Accepts:
///   - the page file itself
///   - the page's detected codebehind (when non-empty)
///   - the conventional `<page>.vb` / `<page>.cs` sibling for an `.aspx`
///     page that did not resolve a codebehind (e.g. the page inherits
///     `System.Web.UI.Page` directly)
///
/// Rejects everything else. The prior version allowed
/// `flag_path.contains(codebehind.unwrap_or(""))` which silently matched
/// *every* file in the project whenever `codebehind` was `None`, so the
/// first page dossier on a project with unresolved codebehinds dumped
/// the entire project-wide flag list into one page's section.
pub(crate) fn flag_belongs_to_page(flag_path: &str, page_path: &str, codebehind: Option<&str>) -> bool {
    if flag_path == page_path {
        return true;
    }
    if let Some(cb) = codebehind
        && !cb.is_empty()
        && flag_path == cb
    {
        return true;
    }
    if page_path.ends_with(".aspx")
        && (flag_path == format!("{}.vb", page_path) || flag_path == format!("{}.cs", page_path))
    {
        return true;
    }
    false
}
