//! Extracted analyzer: web config.
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

/// Extract web.config inventory: appSettings, connectionStrings, handlers,
/// modules, customErrors, compilation, sessionState, pages.
pub(crate) fn extract_webconfig_inventory(
    web_config: &str,
    code_files: &[(&str, &str)],
) -> WebConfigInventory {
    // ── appSettings ──
    let appsettings_section = extract_xml_section(web_config, "appSettings");
    let mut app_settings: Vec<AppSettingEntry> = Vec::new();
    for cap in WC_ADD_KEY_RE.captures_iter(&appsettings_section) {
        let key = cap[1].to_string();
        let raw_value = &cap[2];
        let value_preview = mask_sensitive_value(&key, raw_value);
        let used_by = find_config_references(&key, "AppSettings", code_files);
        app_settings.push(AppSettingEntry {
            key,
            value_preview,
            used_by,
        });
    }

    // ── connectionStrings ──
    let conn_section = extract_xml_section(web_config, "connectionStrings");
    let mut connection_strings: Vec<ConnectionStringEntry> = Vec::new();
    for cap in WC_CONN_RE.captures_iter(&conn_section) {
        let name = cap[1].to_string();
        let cs_value = &cap[2];
        let provider = cap
            .get(3)
            .map_or_else(|| infer_provider(cs_value), |m| m.as_str().to_string());
        let has_integrated_security = cs_value.to_lowercase().contains("integrated security=true")
            || cs_value.to_lowercase().contains("trusted_connection=true");
        let used_by = find_config_references(&name, "ConnectionStrings", code_files);
        connection_strings.push(ConnectionStringEntry {
            name,
            provider,
            has_integrated_security,
            used_by,
        });
    }

    // ── httpHandlers / system.webServer handlers ──
    let handler_section = extract_xml_section(web_config, "httpHandlers")
        + &extract_xml_section(web_config, "handlers");
    let http_handlers: Vec<HandlerRegistration> = WC_HANDLER_RE
        .captures_iter(&handler_section)
        .map(|cap| HandlerRegistration {
            verb: cap[1].to_string(),
            path: cap[2].to_string(),
            handler_type: cap[3].to_string(),
        })
        .collect();

    // ── httpModules / system.webServer modules ──
    let module_section = extract_xml_section(web_config, "httpModules")
        + &extract_xml_section(web_config, "modules");
    let http_modules: Vec<ModuleRegistration> = WC_MODULE_RE
        .captures_iter(&module_section)
        .map(|cap| ModuleRegistration {
            name: cap[1].to_string(),
            module_type: cap[2].to_string(),
        })
        .collect();

    // ── customErrors ──
    let custom_errors = {
        let ce_section = extract_xml_section(web_config, "customErrors");
        WC_CE_RE.captures(&ce_section).map(|cap| {
            let redirects: Vec<(String, String)> = WC_ERROR_RE
                .captures_iter(&ce_section)
                .map(|ec| (ec[1].to_string(), ec[2].to_string()))
                .collect();
            CustomErrorConfig {
                mode: cap[1].to_string(),
                default_redirect: cap.get(2).map(|m| m.as_str().to_string()),
                status_redirects: redirects,
            }
        })
    };

    // ── compilation ──
    let compilation = {
        WC_COMP_RE.captures(web_config).map(|cap| {
            let attrs = &cap[1];
            let debug = attrs.contains(r#"debug="true""#);
            let target_framework = WC_TF_RE.captures(attrs).map(|c| c[1].to_string());
            let comp_section = extract_xml_section(web_config, "compilation");
            let assemblies: Vec<String> = WC_ASM_RE
                .captures_iter(&comp_section)
                .map(|c| c[1].to_string())
                .collect();
            CompilationConfig {
                debug,
                target_framework,
                assemblies,
            }
        })
    };

    // ── sessionState ──
    let session_state = {
        WC_SS_RE.captures(web_config).map(|cap| {
            let attrs = &cap[1];
            SessionStateConfig {
                mode: WC_MODE_RE
                    .captures(attrs)
                    .map_or("InProc".into(), |c| c[1].to_string()),
                timeout_minutes: WC_TIMEOUT_RE
                    .captures(attrs)
                    .and_then(|c| c[1].parse().ok()),
                cookieless: WC_COOKIELESS_RE.captures(attrs).map(|c| c[1].to_string()),
                custom_provider: WC_PROVIDER_RE.captures(attrs).map(|c| c[1].to_string()),
            }
        })
    };

    // ── pages ──
    let pages_config = {
        WC_PAGES_RE.captures(web_config).map(|cap| {
            let attrs = &cap[1];
            let pages_section = extract_xml_section(web_config, "pages");
            PagesConfig {
                theme: WC_THEME_RE.captures(attrs).map(|c| c[1].to_string()),
                master_page_file: WC_MP_RE.captures(attrs).map(|c| c[1].to_string()),
                namespaces: WC_NS_RE
                    .captures_iter(&pages_section)
                    .map(|c| c[1].to_string())
                    .collect(),
                controls: WC_CTRL_RE
                    .captures_iter(&pages_section)
                    .map(|c| format!("{}:{}", &c[1], &c[2]))
                    .collect(),
            }
        })
    };

    WebConfigInventory {
        app_settings,
        connection_strings,
        http_handlers,
        http_modules,
        custom_errors,
        compilation,
        session_state,
        pages_config,
    }
}

/// Helper: extract a named XML section (tag body, non-greedy).
///
/// Uses a plain string search instead of compiling a new `Regex` on every call
/// (this helper is invoked ~10 times per `extract_webconfig_inventory` call).
/// XML section tag names are always simple ASCII identifiers so case-folding and
/// string comparison is sufficient.
pub(crate) fn extract_xml_section(xml: &str, tag: &str) -> String {
    let xml_lower = xml.to_ascii_lowercase();
    let tag_lower = tag.to_ascii_lowercase();

    // Find opening tag: `<tag_lower` followed by either `>` or whitespace (attributes)
    let open_prefix = format!("<{tag_lower}");
    let Some(open_start) = xml_lower.find(open_prefix.as_str()) else {
        return String::new();
    };
    // Advance past the tag name to the first `>`
    let Some(open_end_rel) = xml_lower[open_start..].find('>') else {
        return String::new();
    };
    let body_start = open_start + open_end_rel + 1;

    // Find closing tag
    let close_tag = format!("</{tag_lower}>");
    let Some(close_start_rel) = xml_lower[body_start..].find(close_tag.as_str()) else {
        return String::new();
    };
    let body_end = body_start + close_start_rel;

    xml[body_start..body_end].to_string()
}

/// Mask potentially sensitive config values (API keys, passwords, etc.)
pub(crate) fn mask_sensitive_value(key: &str, value: &str) -> String {
    let k = key.to_lowercase();
    let sensitive = k.contains("key")
        || k.contains("secret")
        || k.contains("password")
        || k.contains("token")
        || k.contains("apikey")
        || k.contains("connectionstring");
    if sensitive && value.len() > 6 {
        format!("{}...", &value[..6])
    } else if value.len() > 30 {
        format!("{}...", &value[..30])
    } else {
        value.to_string()
    }
}

/// Infer ADO.NET provider from connection string content.
pub(crate) fn infer_provider(cs: &str) -> String {
    let lower = cs.to_lowercase();
    if lower.contains("sqloledb") || lower.contains("data source=") {
        "System.Data.SqlClient".into()
    } else if lower.contains("mysql") {
        "MySql.Data.MySqlClient".into()
    } else if lower.contains("npgsql") {
        "Npgsql".into()
    } else if lower.contains("oracle") {
        "System.Data.OracleClient".into()
    } else {
        "System.Data.SqlClient".into()
    }
}

/// Find code files that reference a config key via ConfigurationManager.
pub(crate) fn find_config_references(
    key: &str,
    section: &str,
    code_files: &[(&str, &str)],
) -> Vec<String> {
    let patterns = [
        format!(r#"ConfigurationManager.{section}["{key}"]"#),
        format!(r#"WebConfigurationManager.{section}["{key}"]"#),
        format!(r#"{section}["{key}"]"#),
    ];
    let mut found = Vec::new();
    for &(path, content) in code_files {
        if patterns.iter().any(|p| content.contains(p.as_str())) {
            found.push(path.to_string());
        }
    }
    found
}
