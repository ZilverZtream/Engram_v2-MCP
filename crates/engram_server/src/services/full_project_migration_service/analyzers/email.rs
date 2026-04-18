//! Extracted analyzer: email.
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


pub(crate) fn detect_email_patterns(
    code_files: &[(&str, &str)],
    web_config: Option<&str>,
) -> EmailPatternReport {
    static SMTP_CLIENT_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)\b(?:New\s+)?SmtpClient\s*[\(\.]").expect("valid regex")
    });
    static MAIL_MESSAGE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)\b(?:New\s+)?MailMessage\s*\(").expect("valid regex")
    });
    static WEB_MAIL_RE: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"(?i)\bSystem\.Web\.Mail\b").expect("valid regex"));
    static ATTACHMENT_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)\b(?:New\s+)?Attachment\s*\(").expect("valid regex")
    });
    static ALTERNATE_VIEW_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)\bAlternateView\.CreateAlternateViewFromString\s*\(").expect("valid regex")
    });
    static CDO_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)CreateObject\s*\(\s*"CDO\.Message"\s*\)"#).expect("valid regex")
    });
    static SMTP_CONFIG_HOST_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)<network\s+host\s*=\s*"([^"]*)""#).expect("valid regex")
    });
    static SMTP_CONFIG_PORT_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)<network[^>]*port\s*=\s*"(\d+)""#).expect("valid regex")
    });
    static SMTP_CONFIG_FROM_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)<smtp\s+[^>]*from\s*=\s*"([^"]*)""#).expect("valid regex")
    });

    let mut email_patterns: Vec<EmailPattern> = Vec::new();
    let mut uses_html = false;
    let mut uses_attachments = false;
    let mut uses_cdo = false;
    let mut uses_web_mail = false;
    let mut email_files: Vec<String> = Vec::new();

    for (path, content) in code_files {
        let mut file_patterns: Vec<(&str, &str, usize)> = Vec::new();

        let smtp_count = SMTP_CLIENT_RE.find_iter(content).count();
        if smtp_count > 0 {
            file_patterns.push(("SmtpClient", "IEmailSender / SendGrid SDK", smtp_count));
        }
        let mm_count = MAIL_MESSAGE_RE.find_iter(content).count();
        if mm_count > 0 {
            file_patterns.push(("MailMessage", "IEmailSender with Razor templates", mm_count));
        }
        let wm_count = WEB_MAIL_RE.find_iter(content).count();
        if wm_count > 0 {
            file_patterns.push(("System.Web.Mail", "IEmailSender (obsolete API)", wm_count));
            uses_web_mail = true;
        }
        let cdo_count = CDO_RE.find_iter(content).count();
        if cdo_count > 0 {
            file_patterns.push(("CDO.Message", "IEmailSender (COM object)", cdo_count));
            uses_cdo = true;
        }

        if ATTACHMENT_RE.is_match(content) {
            uses_attachments = true;
        }
        if ALTERNATE_VIEW_RE.is_match(content) {
            uses_html = true;
        }

        if !file_patterns.is_empty() {
            email_files.push(path.to_string());
            for (ptype, modern, count) in file_patterns {
                email_patterns.push(EmailPattern {
                    file_path: path.to_string(),
                    pattern_type: ptype.to_string(),
                    count,
                    modern_equivalent: modern.to_string(),
                });
            }
        }
    }
    email_files.sort();
    email_files.dedup();

    // Parse SMTP config from web.config
    let smtp_config = web_config.and_then(|wc| {
        if !wc.to_lowercase().contains("<smtp") && !wc.to_lowercase().contains("<network") {
            return None;
        }
        let host = SMTP_CONFIG_HOST_RE.captures(wc).map(|c| c[1].to_string());
        let port = SMTP_CONFIG_PORT_RE
            .captures(wc)
            .and_then(|c| c[1].parse().ok());
        let from = SMTP_CONFIG_FROM_RE.captures(wc).map(|c| c[1].to_string());
        let uses_credentials = wc.to_lowercase().contains("username=")
            || wc.to_lowercase().contains("defaultcredentials");
        let uses_ssl =
            wc.to_lowercase().contains("enablessl") || wc.to_lowercase().contains("ssl=\"true\"");
        Some(SmtpConfig {
            host,
            port,
            from_address: from,
            uses_credentials,
            uses_ssl,
        })
    });

    let has_email = !email_patterns.is_empty();

    EmailPatternReport {
        has_email,
        email_patterns,
        smtp_config,
        total_email_files: email_files.len(),
        uses_html_email: uses_html,
        uses_attachments,
        uses_legacy_cdo: uses_cdo,
        uses_legacy_web_mail: uses_web_mail,
    }
}
