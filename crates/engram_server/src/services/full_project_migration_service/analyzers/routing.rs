//! Extracted analyzer: routing.
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

pub(crate) fn extract_url_routing(
    web_config: Option<&str>,
    global_asax_content: &str,
    code_files: &[(&str, &str)],
) -> UrlRoutingInventory {
    static REWRITE_RULE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?is)<rule\s+name="([^"]*)"[^>]*>.*?<match\s+url="([^"]*)"[^/]*/?>.*?<action\s+type="(\w+)"\s+url="([^"]*)"[^/]*/?>.*?</rule>"#).expect("valid regex")
    });
    static URL_MAPPING_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)<add\s+url="([^"]*)"\s+mappedUrl="([^"]*)"\s*/>"#).expect("valid regex")
    });
    static MAP_PAGE_ROUTE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)\.MapPageRoute\s*\(\s*"([^"]*)",\s*"([^"]*)",\s*"([^"]*)""#)
            .expect("valid regex")
    });
    static REWRITE_PATH_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(
            r#"(?i)(?:HttpContext\.Current|Context|HttpContext)\.RewritePath\s*\(\s*"([^"]*)""#,
        )
        .expect("valid regex")
    });
    static REDIRECT_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)Response\.Redirect(Permanent)?\s*\(\s*"([^"]*)""#).expect("valid regex")
    });
    static SERVER_TRANSFER_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)Server\.Transfer\s*\(\s*"([^"]*)""#).expect("valid regex")
    });
    static FRIENDLY_URL_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)FriendlyUrl|FriendlyUrlSettings|EnableFriendlyUrls").expect("valid regex")
    });

    let mut rewrite_rules = Vec::new();
    let mut page_routes = Vec::new();
    let mut url_mappings = Vec::new();
    let mut rewrite_path_calls = Vec::new();
    let mut redirects = Vec::new();
    let mut server_transfers = Vec::new();
    let mut has_friendly_urls = false;

    // Parse web.config
    if let Some(wc) = web_config {
        for cap in REWRITE_RULE_RE.captures_iter(wc) {
            let name = cap[1].to_string();
            let pattern = cap[2].to_string();
            let action = cap[3].to_string();
            let target = cap[4].to_string();
            let modern = build_modern_route_equivalent(&pattern, &target, &action);
            rewrite_rules.push(UrlRewriteRule {
                rule_name: name,
                match_pattern: pattern,
                action_type: action,
                target_url: target,
                modern_equivalent: modern,
            });
        }

        for cap in URL_MAPPING_RE.captures_iter(wc) {
            url_mappings.push(UrlMapping {
                friendly_url: cap[1].to_string(),
                mapped_url: cap[2].to_string(),
            });
        }

        if FRIENDLY_URL_RE.is_match(wc) {
            has_friendly_urls = true;
        }
    }

    // Parse Global.asax for MapPageRoute calls
    for cap in MAP_PAGE_ROUTE_RE.captures_iter(global_asax_content) {
        let route_name = cap[1].to_string();
        let pattern = cap[2].to_string();
        let page = cap[3].to_string();
        let modern = format!("app.MapGet(\"/{pattern}\", ...)");
        page_routes.push(PageRoute {
            route_name,
            url_pattern: pattern,
            physical_page: page,
            modern_equivalent: modern,
        });
    }

    // Scan all code files
    let all_content: Vec<(&str, &str)> = code_files
        .iter()
        .copied()
        .chain(std::iter::once(("Global.asax.vb", global_asax_content)))
        .collect();

    for (path, content) in &all_content {
        for (line_num, line) in content.lines().enumerate() {
            if let Some(cap) = REWRITE_PATH_RE.captures(line) {
                rewrite_path_calls.push(RewritePathCall {
                    file_path: path.to_string(),
                    target_path: cap[1].to_string(),
                    line_number: (line_num + 1) as u32,
                });
            }
            if let Some(cap) = REDIRECT_RE.captures(line) {
                let is_permanent = cap.get(1).is_some();
                redirects.push(RedirectEntry {
                    file_path: path.to_string(),
                    target_url: cap[2].to_string(),
                    is_permanent,
                });
            }
            if let Some(cap) = SERVER_TRANSFER_RE.captures(line) {
                server_transfers.push(ServerTransferEntry {
                    file_path: path.to_string(),
                    target_page: cap[1].to_string(),
                });
            }
        }

        if FRIENDLY_URL_RE.is_match(content) {
            has_friendly_urls = true;
        }
    }

    let total = rewrite_rules.len() + page_routes.len() + url_mappings.len();

    UrlRoutingInventory {
        rewrite_rules,
        page_routes,
        url_mappings,
        rewrite_path_calls,
        redirects,
        server_transfers,
        has_friendly_urls,
        total_url_patterns: total,
    }
}

pub(crate) fn build_modern_route_equivalent(
    pattern: &str,
    target: &str,
    action_type: &str,
) -> String {
    // Convert IIS rewrite regex to ASP.NET Core endpoint pattern
    let route = pattern
        .replace(r"\d+", "{id:int}")
        .replace(r"(\d+)", "{id}")
        .replace(r"([^/]+)", "{slug}")
        .replace("^", "")
        .replace("$", "");
    let _ = target; // target is the rewrite destination
    match action_type.to_lowercase().as_str() {
        "redirect" | "redirectpermanent" => {
            format!("app.MapGet(\"/{route}\", () => Results.Redirect(\"{target}\"))")
        }
        _ => format!("app.MapGet(\"/{route}\", ...)"),
    }
}
