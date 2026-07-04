//! Extracted analyzer: global asax.
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

pub(crate) fn extract_global_asax_info(
    markup_content: &str,
    codebehind_content: &str,
) -> GlobalAsaxSummary {
    use regex::Regex;

    let combined = if codebehind_content.is_empty() {
        markup_content.to_string()
    } else {
        codebehind_content.to_string()
    };

    if combined.trim().is_empty() {
        return GlobalAsaxSummary {
            has_global_asax: false,
            codebehind_class: None,
            lifecycle_events: vec![],
            startup_registrations: vec![],
            modern_mapping: vec![],
        };
    }

    // Extract class name
    let codebehind_class = ASAX_CLASS_RE.captures(&combined).map(|c| c[1].to_string());

    // Event methods to look for
    let event_names = [
        ("Application_Start", "Program.cs builder setup + app.Run()"),
        (
            "Application_OnStart",
            "Program.cs builder setup + app.Run()",
        ),
        (
            "Application_End",
            "IHostApplicationLifetime.ApplicationStopping",
        ),
        (
            "Application_Error",
            "app.UseExceptionHandler() + ProblemDetails",
        ),
        (
            "Application_OnError",
            "app.UseExceptionHandler() + ProblemDetails",
        ),
        ("Session_Start", "Middleware + ISession configuration"),
        ("Session_OnStart", "Middleware + ISession configuration"),
        ("Session_End", "IHostedService background task"),
        ("Session_OnEnd", "IHostedService background task"),
        ("Application_BeginRequest", "app.Use() middleware"),
        (
            "Application_EndRequest",
            "app.Use() middleware (response phase)",
        ),
        ("Application_AuthenticateRequest", "app.UseAuthentication()"),
        (
            "Application_PostAuthenticateRequest",
            "Custom auth middleware after UseAuthentication",
        ),
        ("Application_AuthorizeRequest", "app.UseAuthorization()"),
        (
            "Application_AcquireRequestState",
            "ISession / IDistributedCache middleware",
        ),
    ];

    let mut lifecycle_events = Vec::new();
    for (event_name, modern_equiv) in &event_names {
        let pattern = format!(
            r"(?si)(Sub|void|Handles)\s+{}\b(.*?)(?:End\s+Sub|(?=\b(Sub|void|Protected|Private|Public)\b)|\z)",
            regex::escape(event_name)
        );
        if let Ok(re) = Regex::new(&pattern)
            && let Some(cap) = re.captures(&combined)
        {
            let body = cap.get(2).map_or("", |m| m.as_str());
            let line_count = body.lines().count();
            let key_actions = extract_key_actions(body);
            lifecycle_events.push(GlobalLifecycleEvent {
                event_name: event_name.to_string(),
                line_count,
                key_actions,
                modern_equivalent: modern_equiv.to_string(),
            });
        }
    }

    // Detect startup registrations
    let mut startup_registrations = Vec::new();
    let reg_patterns: &[(&str, &str, &str)] = &[
        (
            r"RouteConfig\.RegisterRoutes|RouteTable\.Routes",
            "routing",
            "app.MapControllerRoute / app.MapBlazorHub",
        ),
        (
            r"BundleConfig\.RegisterBundles|BundleTable\.Bundles",
            "bundling",
            "Vite/Webpack or ASP.NET Core bundling",
        ),
        (
            r"AreaRegistration\.RegisterAllAreas",
            "areas",
            "app.MapAreaControllerRoute",
        ),
        (
            r"GlobalConfiguration\.Configure|WebApiConfig\.Register",
            "webapi",
            "app.MapControllers / Minimal API",
        ),
        (
            r"Container\.Register|kernel\.Bind|builder\.Register|UnityConfig|Ninject|Autofac",
            "di",
            "builder.Services (built-in DI)",
        ),
        (
            r"GlobalFilters\.Filters\.Add",
            "filters",
            "app.AddControllersWithViews + filter options",
        ),
        (
            r"log4net|NLog|Serilog",
            "logging",
            "builder.Logging / Serilog integration",
        ),
    ];
    for &(pattern, reg_type, detail) in reg_patterns {
        if let Ok(re) = Regex::new(pattern)
            && re.is_match(&combined)
        {
            startup_registrations.push(StartupRegistration {
                registration_type: reg_type.to_string(),
                detail: detail.to_string(),
            });
        }
    }

    // Build modern mapping
    let mut modern_mapping = Vec::new();
    if !lifecycle_events.is_empty() {
        modern_mapping.push(ModernMapping {
            legacy: "Application_Start".into(),
            modern: "Program.cs service registration + middleware pipeline".into(),
        });
        modern_mapping.push(ModernMapping {
            legacy: "Session_Start / Session_End".into(),
            modern: "ISession middleware or custom middleware".into(),
        });
        modern_mapping.push(ModernMapping {
            legacy: "Application_Error".into(),
            modern: "UseExceptionHandler + ProblemDetails".into(),
        });
        modern_mapping.push(ModernMapping {
            legacy: "Application_BeginRequest / EndRequest".into(),
            modern: "Custom middleware pipeline".into(),
        });
    }

    GlobalAsaxSummary {
        has_global_asax: true,
        codebehind_class,
        lifecycle_events,
        startup_registrations,
        modern_mapping,
    }
}

/// Extract key actions from a method body (routing, bundling, DI, state init, etc.)
pub(crate) fn extract_key_actions(body: &str) -> Vec<String> {
    let mut actions = Vec::new();
    let checks: &[(&str, &str)] = &[
        ("RouteConfig", "RouteConfig registration"),
        ("BundleConfig", "BundleConfig registration"),
        ("AreaRegistration", "Area registration"),
        ("GlobalConfiguration", "Web API configuration"),
        ("Container.Register", "DI container registration"),
        ("kernel.Bind", "DI (Ninject) binding"),
        ("builder.Register", "DI (Autofac) registration"),
        ("Application(", "Application state initialization"),
        ("Session(", "Session state initialization"),
        ("Response.Redirect", "Redirect on error/event"),
        ("Server.Transfer", "Server.Transfer"),
        ("log4net", "Logging (log4net)"),
        ("NLog", "Logging (NLog)"),
        ("Serilog", "Logging (Serilog)"),
        ("Exception", "Exception handling"),
        ("HttpContext.Current", "HttpContext usage"),
    ];
    for &(pattern, action) in checks {
        if body.contains(pattern) {
            actions.push(action.to_string());
        }
    }
    actions
}
