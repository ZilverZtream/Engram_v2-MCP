//! Extracted analyzer: dependencies.
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

pub(crate) fn build_dependency_inventory(
    project_refs: &[ProjectReferenceBundle],
) -> DependencyInventory {
    let mut target_frameworks: Vec<String> = Vec::new();
    let mut all_packages: Vec<NuGetPackageInfo> = Vec::new();
    let mut all_assemblies: Vec<AssemblyRefInfo> = Vec::new();
    let mut proj_refs: Vec<ProjectRefInfo> = Vec::new();
    let mut seen_packages: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut seen_assemblies: std::collections::HashSet<String> = std::collections::HashSet::new();

    for prb in project_refs {
        if let Some(ref tf) = prb.target_framework
            && !target_frameworks.contains(tf)
        {
            target_frameworks.push(tf.clone());
        }

        for pkg in &prb.package_references {
            if seen_packages.contains(&pkg.name.to_lowercase()) {
                continue;
            }
            seen_packages.insert(pkg.name.to_lowercase());

            let (modern, version, notes, category) = lookup_modern_replacement(&pkg.name);
            let is_framework =
                pkg.name.starts_with("System.") || pkg.name.starts_with("Microsoft.");

            if is_framework && pkg.version.is_none() {
                // This is an assembly reference, not a NuGet package
                if seen_assemblies.insert(pkg.name.to_lowercase()) {
                    let (asm_modern, removal) = lookup_assembly_replacement(&pkg.name);
                    all_assemblies.push(AssemblyRefInfo {
                        assembly_name: pkg.name.clone(),
                        is_framework: true,
                        modern_equivalent: asm_modern.map(|s| s.to_string()),
                        removal_reason: removal.map(|s| s.to_string()),
                    });
                }
            } else {
                all_packages.push(NuGetPackageInfo {
                    name: pkg.name.clone(),
                    version: pkg.version.clone(),
                    modern_replacement: modern.map(|s| s.to_string()),
                    modern_version: version.map(|s| s.to_string()),
                    migration_notes: notes.map(|s| s.to_string()),
                    category: category.to_string(),
                });
            }
        }

        for asm in &prb.assembly_references {
            if seen_assemblies.contains(&asm.to_lowercase()) {
                continue;
            }
            seen_assemblies.insert(asm.to_lowercase());
            let is_fw =
                asm.starts_with("System.") || asm.starts_with("Microsoft.") || asm == "mscorlib";
            let (modern, removal) = lookup_assembly_replacement(asm);
            all_assemblies.push(AssemblyRefInfo {
                assembly_name: asm.clone(),
                is_framework: is_fw,
                modern_equivalent: modern.map(|s| s.to_string()),
                removal_reason: removal.map(|s| s.to_string()),
            });
        }

        for dep in &prb.project_dependencies {
            proj_refs.push(ProjectRefInfo {
                project_name: dep.rsplit(['/', '\\']).next().unwrap_or(dep).to_string(),
                project_path: dep.clone(),
                target_framework: prb.target_framework.clone(),
            });
        }
    }

    let framework_assemblies: Vec<String> = all_assemblies
        .iter()
        .filter(|a| a.is_framework)
        .map(|a| a.assembly_name.clone())
        .collect();
    let third_party_assemblies: Vec<String> = all_assemblies
        .iter()
        .filter(|a| !a.is_framework)
        .map(|a| a.assembly_name.clone())
        .collect();
    let with_replacement = all_packages
        .iter()
        .filter(|p| p.modern_replacement.is_some())
        .count();
    let without_replacement = all_packages.len() - with_replacement;

    DependencyInventory {
        total_packages: all_packages.len(),
        total_assemblies: all_assemblies.len(),
        packages_with_known_replacement: with_replacement,
        packages_without_replacement: without_replacement,
        framework_assemblies,
        third_party_assemblies,
        target_frameworks,
        nuget_packages: all_packages,
        assembly_references: all_assemblies,
        project_references: proj_refs,
        legacy_packages: Vec::new(), // populated separately from packages.config
        binding_redirects: Vec::new(), // populated separately from web.config
    }
}

/// Returns (modern_replacement, modern_version, migration_notes, category)
pub(crate) fn lookup_modern_replacement(
    package: &str,
) -> (
    Option<&'static str>,
    Option<&'static str>,
    Option<&'static str>,
    &'static str,
) {
    match package.to_lowercase().as_str() {
        "entityframework" => (
            Some("Microsoft.EntityFrameworkCore"),
            Some("8.0+"),
            Some("Different DbContext API; migrations differ"),
            "ORM",
        ),
        "newtonsoft.json" => (
            Some("System.Text.Json (or keep Newtonsoft)"),
            Some("8.0+"),
            Some("Compatible; System.Text.Json is built-in but API differs"),
            "Serialization",
        ),
        "telerik.web.ui" => (
            Some("Telerik.UI.for.Blazor"),
            None,
            Some("Commercial, completely different API surface"),
            "UI Controls",
        ),
        "devexpress.web" | "devexpress.web.v23.1" => (
            Some("DevExpress.Blazor"),
            None,
            Some("Commercial, different API"),
            "UI Controls",
        ),
        "infragistics.web" | "infragistics4.web" => (
            Some("IgniteUI.Blazor"),
            None,
            Some("Commercial"),
            "UI Controls",
        ),
        "log4net" => (
            Some("Serilog or NLog"),
            Some("3.0+"),
            Some("Similar patterns; Serilog preferred for structured logging"),
            "Logging",
        ),
        "nlog" => (
            Some("NLog"),
            Some("5.0+"),
            Some("Already .NET Core compatible"),
            "Logging",
        ),
        "serilog" => (
            Some("Serilog"),
            Some("3.0+"),
            Some("Already compatible"),
            "Logging",
        ),
        "unity" => (
            Some("Microsoft.Extensions.DependencyInjection"),
            Some("8.0+"),
            Some("Built-in DI in ASP.NET Core"),
            "DI",
        ),
        "autofac" => (
            Some("Autofac"),
            Some("7.0+"),
            Some(".NET Core compatible"),
            "DI",
        ),
        "structuremap" => (
            Some("Microsoft.Extensions.DependencyInjection or Lamar"),
            None,
            Some("StructureMap discontinued; Lamar is successor"),
            "DI",
        ),
        "system.data.sqlclient" => (
            Some("Microsoft.Data.SqlClient"),
            Some("5.0+"),
            Some("Namespace change; connection string compatible"),
            "Data",
        ),
        "microsoft.practices.enterpriselibrary.data" | "enterpriselibrary.data" => (
            Some("Microsoft.Data.SqlClient + Dapper"),
            None,
            Some("Replace per-block; no single equivalent"),
            "Data",
        ),
        "npoi" => (
            Some("NPOI"),
            Some("2.6+"),
            Some("Compatible; or consider ClosedXML"),
            "Office",
        ),
        "epplus" => (
            Some("EPPlus"),
            Some("7.0+"),
            Some("License changed to commercial in v5+"),
            "Office",
        ),
        "itextsharp" => (
            Some("itext7 or QuestPDF"),
            None,
            Some("License changed; QuestPDF is MIT"),
            "PDF",
        ),
        "crystaldecisions.crystalreports.engine" => (
            None,
            None,
            Some("No .NET Core port; consider SSRS, FastReport, or Telerik Reporting"),
            "Reporting",
        ),
        "microsoft.reportviewer.webforms"
        | "microsoft.reportingservices.reportviewercontrol.webforms" => (
            Some("Microsoft.Reporting.NETCore"),
            Some("16.0+"),
            Some("Limited .NET Core support"),
            "Reporting",
        ),
        "microsoft.aspnet.signalr" => (
            Some("Microsoft.AspNetCore.SignalR"),
            Some("8.0+"),
            Some("Different hub API; requires rewrite"),
            "RealTime",
        ),
        "system.web.optimization" => (
            None,
            None,
            Some("Removed; use Vite, Webpack, or esbuild for bundling"),
            "Build",
        ),
        "microsoft.owin" | "owin" => (
            None,
            None,
            Some("ASP.NET Core has native middleware pipeline"),
            "Middleware",
        ),
        "nhibernate" => (
            Some("NHibernate or EF Core"),
            Some("5.4+"),
            Some(".NET Core compatible"),
            "ORM",
        ),
        "dapper" => (
            Some("Dapper"),
            Some("2.1+"),
            Some("Already compatible"),
            "ORM",
        ),
        "fluentvalidation" => (
            Some("FluentValidation"),
            Some("11.0+"),
            Some("Already compatible"),
            "Validation",
        ),
        "automapper" => (
            Some("AutoMapper or Mapster"),
            Some("13.0+"),
            Some("Already compatible; Mapster is faster alternative"),
            "Mapping",
        ),
        "mediatr" => (
            Some("MediatR"),
            Some("12.0+"),
            Some("Already compatible"),
            "Patterns",
        ),
        "hangfire" | "hangfire.core" => (
            Some("Hangfire"),
            Some("1.8+"),
            Some("Already compatible"),
            "BackgroundJobs",
        ),
        "quartz" | "quartz.net" => (
            Some("Quartz.NET"),
            Some("3.8+"),
            Some("Already compatible"),
            "BackgroundJobs",
        ),
        "stackexchange.redis" => (
            Some("StackExchange.Redis"),
            Some("2.7+"),
            Some("Already compatible"),
            "Cache",
        ),
        "microsoft.aspnet.webapi" | "microsoft.aspnet.webapi.core" => (
            Some("Microsoft.AspNetCore.Mvc"),
            Some("8.0+"),
            Some("Unified in ASP.NET Core; different routing and DI"),
            "Web",
        ),
        "microsoft.aspnet.mvc" => (
            Some("Microsoft.AspNetCore.Mvc"),
            Some("8.0+"),
            Some("Different routing, DI, filters"),
            "Web",
        ),
        "ajaxcontroltoolkit" => (
            None,
            None,
            Some("No .NET Core port; use MudBlazor or JavaScript components"),
            "UI Controls",
        ),
        "antlr" | "antlr3.runtime" => (
            Some("Antlr4.Runtime.Standard"),
            Some("4.13+"),
            Some("Different API"),
            "Parsing",
        ),
        "webgrease" => (None, None, Some("Removed; use modern bundler"), "Build"),
        "microsoft.web.infrastructure" => (None, None, Some("Built into ASP.NET Core"), "Web"),
        "microsoft.aspnet.web.optimization" => {
            (None, None, Some("Use Vite/Webpack for bundling"), "Build")
        }
        "dotnetopenauth" => (
            Some("Microsoft.AspNetCore.Authentication.OAuth"),
            Some("8.0+"),
            Some("Built-in OAuth in ASP.NET Core"),
            "Auth",
        ),
        "microsoft.identitymodel.tokens" => (
            Some("Microsoft.IdentityModel.Tokens"),
            Some("7.0+"),
            Some("Already compatible"),
            "Auth",
        ),
        "microsoft.aspnet.identity.core" => (
            Some("Microsoft.AspNetCore.Identity"),
            Some("8.0+"),
            Some("Different API but same concepts"),
            "Auth",
        ),
        "system.web.services" => (None, None, Some("Removed; use Minimal API or gRPC"), "Web"),
        "system.servicemodel" => (
            Some("CoreWCF or gRPC"),
            None,
            Some("Limited WCF in .NET Core; gRPC preferred"),
            "Services",
        ),
        "system.enterpriseservices" => (None, None, Some("No .NET Core equivalent"), "Legacy"),
        "system.directoryservices" => (
            Some("System.DirectoryServices"),
            None,
            Some("Partial support; needs platform-specific shim"),
            "Directory",
        ),
        "system.drawing" => (
            Some("System.Drawing.Common or SkiaSharp"),
            None,
            Some("Linux needs libgdiplus; SkiaSharp is cross-platform"),
            "Graphics",
        ),
        _ => (None, None, None, "Unknown"),
    }
}

pub(crate) fn lookup_assembly_replacement(
    assembly: &str,
) -> (Option<&'static str>, Option<&'static str>) {
    match assembly.to_lowercase().as_str() {
        "system.web" => (
            None,
            Some("Removed in .NET Core — use ASP.NET Core middleware"),
        ),
        "system.web.mvc" => (
            Some("Microsoft.AspNetCore.Mvc"),
            Some("Different routing and DI"),
        ),
        "system.web.services" => (None, Some("Removed — use Minimal API or gRPC")),
        "system.web.extensions" => (
            None,
            Some("Removed — AJAX functionality is built-in to ASP.NET Core"),
        ),
        "system.enterpriseservices" => (None, Some("Removed — no .NET Core equivalent")),
        "system.web.mobile" => (None, Some("Removed — use responsive design")),
        "system.web.routing" => (None, Some("Built into ASP.NET Core endpoint routing")),
        "system.web.abstractions" => (None, Some("Built into ASP.NET Core")),
        "system.web.dynamicdata" => (None, Some("Removed — no equivalent")),
        "system.web.entity" => (None, Some("Use EF Core")),
        "system.web.applicationservices" => (None, Some("Use ASP.NET Core Identity")),
        "microsoft.csharp" => (Some("Microsoft.CSharp"), None),
        "system.configuration" => (
            Some("Microsoft.Extensions.Configuration"),
            Some("Different API"),
        ),
        "system.data" => (Some("System.Data.Common"), None),
        "system.data.sqlclient" => (Some("Microsoft.Data.SqlClient"), Some("Namespace change")),
        _ => (None, None),
    }
}

/// Parse packages.config XML. Handles any attribute order within `<package ... />` elements.
pub(crate) fn parse_packages_config(content: &str) -> Vec<LegacyPackageRef> {
    let mut packages = Vec::new();

    for element in PKG_CONFIG_ELEMENT_RE.captures_iter(content) {
        let attrs = &element[1];

        // id and version are required
        let Some(id_cap) = PKG_ATTR_ID_RE.captures(attrs) else {
            continue;
        };
        let Some(ver_cap) = PKG_ATTR_VER_RE.captures(attrs) else {
            continue;
        };

        let package_id = id_cap[1].to_string();
        let version = ver_cap[1].to_string();
        let target_framework = PKG_ATTR_TFM_RE
            .captures(attrs)
            .map(|c| c[1].to_string())
            .unwrap_or_default();
        let is_dev = PKG_ATTR_DEV_RE.is_match(attrs);

        let modern_replacement = {
            let (repl, _, _, _) = lookup_modern_replacement(&package_id);
            repl.map(|s| s.to_string())
        };

        packages.push(LegacyPackageRef {
            package_id,
            version,
            target_framework,
            is_dev_dependency: is_dev,
            modern_replacement,
        });
    }

    packages
}

pub(crate) fn extract_binding_redirects(web_config: Option<&str>) -> Vec<BindingRedirect> {
    let Some(content) = web_config else {
        return Vec::new();
    };

    let mut redirects = Vec::new();

    for block in DEP_ASSEMBLY_RE.captures_iter(content) {
        let inner = &block[1];

        // Extract assemblyIdentity attributes (any order)
        let Some(name_cap) = ASM_NAME_RE.captures(inner) else {
            continue;
        };
        let assembly_name = name_cap[1].to_string();
        let public_key_token = ASM_PKT_RE.captures(inner).map(|c| c[1].to_string());

        // Extract bindingRedirect attributes (any order)
        let Some(old_cap) = BR_OLD_VER_RE.captures(inner) else {
            continue;
        };
        let Some(new_cap) = BR_NEW_VER_RE.captures(inner) else {
            continue;
        };
        let old_version = old_cap[1].to_string();
        let new_version = new_cap[1].to_string();

        let has_known = lookup_assembly_replacement(&assembly_name).0.is_some();

        redirects.push(BindingRedirect {
            assembly_name,
            old_version_range: old_version,
            new_version,
            public_key_token,
            has_known_replacement: has_known,
        });
    }

    redirects
}
