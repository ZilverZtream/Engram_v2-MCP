//! Extracted analyzer: config transforms.
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


pub(crate) fn parse_config_transforms(transform_files: &[(String, String)]) -> ConfigTransformReport {
    let mut environments: Vec<ConfigEnvironment> = Vec::new();
    let mut total_transforms = 0usize;
    let mut conn_overrides: Vec<(String, String)> = Vec::new();
    let mut debug_overrides: Vec<(String, bool)> = Vec::new();
    let mut setting_overrides: Vec<(String, String, String)> = Vec::new();
    // MIG1/D2: log if value-attribute regex fails to compile.
    let value_attr_re = Regex::new(r#"value\s*=\s*"([^"]*)""#)
        .inspect_err(|e| tracing::warn!(error = %e, "MIG1: config transform value_attr regex compile failed — value extraction disabled"))
        .ok();

    for (path, content) in transform_files {
        // Derive environment name from filename: web.Release.config → Release
        let env_name = path
            .rsplit('/')
            .next()
            .or_else(|| path.rsplit('\\').next())
            .unwrap_or(path)
            .strip_prefix("web.")
            .or_else(|| path.strip_prefix("Web."))
            .unwrap_or(path)
            .strip_suffix(".config")
            .unwrap_or(path)
            .to_string();

        let mut transforms: Vec<ConfigTransform> = Vec::new();

        // Extract all XDT transform operations
        for cap in XDT_TRANSFORM_RE.captures_iter(content) {
            let operation = cap[1].to_string();

            // Find the XML element context around this transform
            let match_pos = cap.get(0).map_or(0, |m| m.start());
            let context_start = content[..match_pos].rfind('<').unwrap_or(0);
            let context_end = content[match_pos..]
                .find('>')
                .map(|p| match_pos + p + 1)
                .unwrap_or(content.len());
            let context = &content[context_start..context_end];

            let key = XDT_LOCATOR_RE.captures(context).map(|c| c[1].to_string());

            // Extract value preview (sanitize sensitive values)
            let value_preview = if context.contains("connectionString") {
                Some("(connection string)".to_string())
            } else if let Some(val_cap) = value_attr_re.as_ref().and_then(|re| re.captures(context))
            {
                let val = val_cap[1].to_string();
                if val.len() > 50 {
                    Some(format!("{}...", &val[..47]))
                } else {
                    Some(val)
                }
            } else {
                None
            };

            // Derive xpath hint from element context AND parent context.
            // Look back in the content to find the parent XML element for nested <add> elements.
            let parent_context = &content[..match_pos];
            let xpath_hint = if context.contains("<appSettings")
                || (context.contains("<add ") && context.contains("key="))
                || parent_context
                    .rfind("<appSettings")
                    .is_some_and(|p| !parent_context[p..].contains("</appSettings"))
            {
                "configuration/appSettings".to_string()
            } else if context.contains("connectionStrings")
                || context.contains("connectionString")
                || parent_context
                    .rfind("<connectionStrings")
                    .is_some_and(|p| !parent_context[p..].contains("</connectionStrings"))
            {
                "configuration/connectionStrings".to_string()
            } else if context.contains("<compilation") {
                "configuration/system.web/compilation".to_string()
            } else if context.contains("<customErrors") {
                "configuration/system.web/customErrors".to_string()
            } else if context.contains("<httpHandlers")
                || context.contains("<handlers")
                || parent_context
                    .rfind("<handlers")
                    .is_some_and(|p| !parent_context[p..].contains("</handlers"))
            {
                "configuration/system.webServer/handlers".to_string()
            } else if context.contains("<httpModules")
                || context.contains("<modules")
                || parent_context
                    .rfind("<modules")
                    .is_some_and(|p| !parent_context[p..].contains("</modules"))
            {
                "configuration/system.webServer/modules".to_string()
            } else if context.contains("<system.webServer") {
                "configuration/system.webServer".to_string()
            } else if context.contains("<system.web") {
                "configuration/system.web".to_string()
            } else {
                "configuration/...".to_string()
            };

            transforms.push(ConfigTransform {
                xpath_hint,
                operation,
                key,
                value_preview,
            });
            total_transforms += 1;
        }

        // Extract connection string overrides
        for cap in XDT_CONNSTR_RE.captures_iter(content) {
            conn_overrides.push((env_name.clone(), cap[1].to_string()));
        }

        // Extract debug flag overrides
        if let Some(cap) = XDT_DEBUG_RE.captures(content) {
            let debug_val = cap[1].eq_ignore_ascii_case("true");
            debug_overrides.push((env_name.clone(), debug_val));
        }

        // Extract app setting overrides
        for cap in XDT_APPSETTING_RE.captures_iter(content) {
            setting_overrides.push((env_name.clone(), cap[1].to_string(), cap[2].to_string()));
        }

        if !transforms.is_empty() {
            environments.push(ConfigEnvironment {
                name: env_name,
                file_path: path.clone(),
                transforms,
            });
        }
    }

    ConfigTransformReport {
        environments,
        total_transforms,
        connection_string_overrides: conn_overrides,
        debug_flag_overrides: debug_overrides,
        app_setting_overrides: setting_overrides,
    }
}
