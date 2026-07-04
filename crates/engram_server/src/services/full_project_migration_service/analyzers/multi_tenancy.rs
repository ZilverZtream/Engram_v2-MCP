//! Extracted analyzer: multi tenancy.
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

pub(crate) fn detect_multi_tenancy(
    web_config: Option<&str>,
    code_files: &[(&str, &str)],
    global_asax_content: Option<&str>,
) -> MultiTenancyReport {
    static TENANT_SESSION_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)Session\s*[\(\[]\s*"(?:TenantId|Tenant|TenantKey|TenantCode|OrganizationId|OrgId|CompanyId|ClientId|SiteId|AccountId|CustomerId)"#).expect("valid regex")
    });
    static TENANT_CONTEXT_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)(?:HttpContext\.Current\.Items|Context\.Items)\s*[\(\[]\s*"(?:TenantId|Tenant|TenantContext|CurrentTenant)"#).expect("valid regex")
    });
    static TENANT_SQL_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)(?:WHERE|AND)\s+(?:\w+\.)?(?:TenantId|TenantID|Tenant_ID|OrgId|OrganizationId|CompanyId|SiteId|AccountId)\s*=\s*(?:@\w+|'\w*'|\?)"#).expect("valid regex")
    });
    static TENANT_PARAM_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)(?:tenantId|tenant_id|orgId|organizationId|companyId|siteId|accountId)\s+(?:As\s+(?:Integer|String|Guid|Long|Int32|Int64)|:\s*(?:int|string|Guid|long))"#).expect("valid regex")
    });
    static TENANT_CONFIG_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)(?:TenantMode|MultiTenancy|TenantProvider|TenantResolution|TenantStrategy|IsTenanted|EnableMultiTenancy)"#).expect("valid regex")
    });
    static TENANT_CONN_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(
            r#"(?i)(?:GetConnectionString|ConnectionString)\s*[\(\[]\s*(?:tenantId|tenant|orgId)"#,
        )
        .expect("valid regex")
    });
    static SUBDOMAIN_TENANT_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)(?:Request\.Url\.Host|Request\.Headers\["X-Tenant"|Request\.Headers\["Host"\]).*(?:Split|Substring|Replace|tenant|org)"#).expect("valid regex")
    });
    static TENANT_MODULE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)class\s+\w*(?:Tenant|MultiTenant|Org)\w*\s*(?::\s*I(?:Http)?Module|Inherits\s+I(?:Http)?Module)"#).expect("valid regex")
    });

    let mut evidence: Vec<TenancyEvidence> = Vec::new();
    let mut files_with_tenant: Vec<String> = Vec::new();
    let mut tenant_filtered_queries = 0usize;
    let mut tenant_resolution: Option<TenantResolution> = None;
    let mut tenant_col_name: Option<String> = None;

    // Scan web.config
    if let Some(wc) = web_config
        && TENANT_CONFIG_RE.is_match(wc)
    {
        evidence.push(TenancyEvidence {
            evidence_type: "config".to_string(),
            file_path: "web.config".to_string(),
            detail: "Tenant configuration key found in web.config".to_string(),
            line_hint: None,
        });
    }

    // Scan Global.asax
    if let Some(ga) = global_asax_content
        && (TENANT_MODULE_RE.is_match(ga) || SUBDOMAIN_TENANT_RE.is_match(ga))
    {
        evidence.push(TenancyEvidence {
            evidence_type: "module".to_string(),
            file_path: "Global.asax".to_string(),
            detail: "Tenant resolution logic in Global.asax".to_string(),
            line_hint: None,
        });
    }

    // Scan code files
    for (path, content) in code_files {
        let mut file_has_tenant = false;

        if TENANT_SESSION_RE.is_match(content) {
            evidence.push(TenancyEvidence {
                evidence_type: "session_storage".to_string(),
                file_path: path.to_string(),
                detail: "Tenant ID stored in/read from Session".to_string(),
                line_hint: None,
            });
            file_has_tenant = true;
        }

        if TENANT_CONTEXT_RE.is_match(content) {
            evidence.push(TenancyEvidence {
                evidence_type: "context_items".to_string(),
                file_path: path.to_string(),
                detail: "Tenant context stored in HttpContext.Items".to_string(),
                line_hint: None,
            });
            file_has_tenant = true;
        }

        let sql_count = TENANT_SQL_RE.find_iter(content).count();
        if sql_count > 0 {
            tenant_filtered_queries += sql_count;
            evidence.push(TenancyEvidence {
                evidence_type: "sql_filter".to_string(),
                file_path: path.to_string(),
                detail: format!("{sql_count} SQL queries filter by tenant column"),
                line_hint: None,
            });
            file_has_tenant = true;
            // Try to extract the most common column name
            if tenant_col_name.is_none()
                && let Some(cap) = TENANT_SQL_RE.captures(content)
            {
                let full_match = cap.get(0).expect("group 0 always present").as_str();
                if let Some(col) = full_match.split('=').next() {
                    let col = col.trim().rsplit('.').next().unwrap_or(col.trim());
                    let col = col.split_whitespace().last().unwrap_or(col);
                    tenant_col_name = Some(col.to_string());
                }
            }
        }

        if TENANT_PARAM_RE.is_match(content) {
            evidence.push(TenancyEvidence {
                evidence_type: "parameter".to_string(),
                file_path: path.to_string(),
                detail: "Method parameter with tenant ID".to_string(),
                line_hint: None,
            });
            file_has_tenant = true;
        }

        if TENANT_CONN_RE.is_match(content) {
            evidence.push(TenancyEvidence {
                evidence_type: "connection_string".to_string(),
                file_path: path.to_string(),
                detail: "Tenant-specific connection string selection".to_string(),
                line_hint: None,
            });
            file_has_tenant = true;
        }

        if SUBDOMAIN_TENANT_RE.is_match(content) {
            evidence.push(TenancyEvidence {
                evidence_type: "subdomain".to_string(),
                file_path: path.to_string(),
                detail: "Subdomain-based tenant resolution".to_string(),
                line_hint: None,
            });
            file_has_tenant = true;
            if tenant_resolution.is_none() {
                tenant_resolution = Some(TenantResolution {
                    mechanism: "subdomain".to_string(),
                    module_class: None,
                    file_path: path.to_string(),
                });
            }
        }

        if TENANT_MODULE_RE.is_match(content) {
            evidence.push(TenancyEvidence {
                evidence_type: "http_module".to_string(),
                file_path: path.to_string(),
                detail: "Tenant resolution IHttpModule".to_string(),
                line_hint: None,
            });
            file_has_tenant = true;
            tenant_resolution = Some(TenantResolution {
                mechanism: "http_module".to_string(),
                module_class: TENANT_MODULE_RE
                    .captures(content)
                    .and_then(|c| c.get(0))
                    .map(|m| m.as_str().to_string()),
                file_path: path.to_string(),
            });
        }

        if file_has_tenant {
            files_with_tenant.push(path.to_string());
        }
    }

    files_with_tenant.sort();
    files_with_tenant.dedup();

    // Classify confidence
    let evidence_types: std::collections::HashSet<&str> =
        evidence.iter().map(|e| e.evidence_type.as_str()).collect();
    let confidence = match evidence_types.len() {
        0 => "none",
        1 => "low",
        2 => "medium",
        _ => "high",
    };

    let is_multi_tenant = !evidence.is_empty();

    // Determine isolation strategy
    let isolation_strategy = if evidence_types.contains("connection_string") {
        Some("separate_db".to_string())
    } else if tenant_filtered_queries > 0 {
        Some("shared_db_shared_schema".to_string())
    } else {
        None
    };

    // Build recommendations
    let mut recommendations = Vec::new();
    if is_multi_tenant {
        recommendations
            .push("Replace tenant resolution module with ASP.NET Core middleware".to_string());
        recommendations
            .push("Use EF Core Global Query Filters for automatic tenant filtering".to_string());
        recommendations
            .push("Register ITenantContext as scoped service (one per request)".to_string());
        if isolation_strategy.as_deref() == Some("separate_db") {
            recommendations
                .push("Use IDbContextFactory<T> with tenant-specific connections".to_string());
        }
        recommendations.push(
            "Move Session-based tenant ID to JWT claims or HttpContext.Items via middleware"
                .to_string(),
        );
        recommendations.push("CRITICAL: Audit ALL SQL queries for tenant filtering — missing ANY filter causes data leak".to_string());
    }

    MultiTenancyReport {
        is_multi_tenant,
        confidence: confidence.to_string(),
        tenant_id_column_name: tenant_col_name,
        isolation_strategy,
        detection_evidence: evidence,
        tenant_resolution,
        tenant_filtered_queries,
        files_with_tenant_logic: files_with_tenant,
        migration_recommendations: recommendations,
    }
}
