//! Strangler Fig migration infrastructure generator.
//!
//! Produces a complete set of artifacts for incremental strangler fig migration
//! from a legacy ASP.NET WebForms application to a modern stack:
//!
//! - **YARP reverse proxy** configuration routing requests to legacy or modern
//! - **Feature flag** setup with per-page toggles
//! - **Routing middleware** with percentage-based rollout support
//! - **Health check** endpoint reporting migration progress

use engram_graph::GraphStore;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::Arc;

/// Complete strangler fig migration infrastructure output.
#[derive(Debug, Clone, Serialize)]
pub struct StranglerFigConfig {
    /// YARP reverse proxy `appsettings.YARP.json` content.
    pub yarp_config: String,
    /// Feature flag configuration (`appsettings.FeatureFlags.json` + middleware).
    pub feature_flags_config: String,
    /// ASP.NET Core routing middleware (`StranglerFigMiddleware.cs`).
    pub routing_middleware: String,
    /// Migration health check endpoint (`MigrationHealthCheck.cs`).
    pub health_check: String,
    /// Complete `Program.cs` registration code for all middleware/services.
    pub program_cs: String,
    /// Step-by-step deployment instructions.
    pub deployment_steps: Vec<String>,
    /// Pages already marked as migrated (have migration insight nodes).
    pub migrated_pages: Vec<String>,
    /// Pages not yet migrated.
    pub unmigrated_pages: Vec<String>,
}

/// Generate the full strangler fig migration configuration from graph data.
///
/// # Arguments
/// * `graph` - shared graph store
/// * `project_id` - project identifier
/// * `legacy_base_url` - base URL of the legacy application (e.g. `http://localhost:5000`)
/// * `modern_base_url` - base URL of the modern application (e.g. `http://localhost:5001`)
pub fn generate_strangler_fig_config(
    graph: &Arc<GraphStore>,
    project_id: &str,
    legacy_base_url: &str,
    modern_base_url: &str,
) -> anyhow::Result<StranglerFigConfig> {
    // 1. Discover all .aspx file nodes
    let all_files = graph.query_nodes(project_id, Some("file"), None, None, 10_000)?;
    let aspx_pages: Vec<String> = all_files
        .iter()
        .filter(|n| {
            let p = n.file_path.as_str();
            p.ends_with(".aspx") && !p.ends_with(".aspx.cs") && !p.ends_with(".aspx.vb")
        })
        .map(|n| n.file_path.as_str().to_string())
        .collect();

    // 2. Check insight nodes for migration status markers
    let insights = graph.query_nodes(project_id, Some("insight"), None, None, 10_000)?;
    let mut migrated_set: BTreeMap<String, bool> = BTreeMap::new();
    for insight in &insights {
        let meta_str = insight
            .metadata
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_default()
            .to_lowercase();
        let name_lower = insight.name.to_lowercase();
        if meta_str.contains("migrated") || name_lower.contains("migrated") {
            // Try to correlate to a page via file_path or name
            let fp = insight.file_path.as_str();
            if fp.ends_with(".aspx") {
                migrated_set.insert(fp.to_string(), true);
            }
        }
    }

    let mut migrated_pages: Vec<String> = Vec::new();
    let mut unmigrated_pages: Vec<String> = Vec::new();
    for page in &aspx_pages {
        if migrated_set.contains_key(page.as_str()) {
            migrated_pages.push(page.clone());
        } else {
            unmigrated_pages.push(page.clone());
        }
    }
    migrated_pages.sort();
    unmigrated_pages.sort();

    // 3. Build all five artifacts
    let yarp_config =
        generate_yarp_config(&aspx_pages, &migrated_set, legacy_base_url, modern_base_url);
    let feature_flags_config = generate_feature_flags(&aspx_pages, &migrated_set);
    let routing_middleware =
        generate_routing_middleware(&aspx_pages, &migrated_set, legacy_base_url);
    let health_check = generate_health_check(&aspx_pages, &migrated_set);
    let program_cs = generate_program_cs(legacy_base_url, modern_base_url);

    // 4. Deployment steps
    let deployment_steps = build_deployment_steps(legacy_base_url, modern_base_url);

    Ok(StranglerFigConfig {
        yarp_config,
        feature_flags_config,
        routing_middleware,
        health_check,
        program_cs,
        deployment_steps,
        migrated_pages,
        unmigrated_pages,
    })
}

// ─── Page name helpers ──────────────────────────────────────────────────────

fn page_name_from_path(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let file_name = normalized.rsplit('/').next().unwrap_or(&normalized);
    file_name
        .strip_suffix(".aspx")
        .unwrap_or(file_name)
        .to_string()
}

fn route_from_path(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    if normalized.starts_with('/') {
        normalized
    } else {
        format!("/{normalized}")
    }
}

// ─── YARP configuration ─────────────────────────────────────────────────────

fn generate_yarp_config(
    pages: &[String],
    migrated: &BTreeMap<String, bool>,
    legacy_url: &str,
    modern_url: &str,
) -> String {
    let mut out = String::with_capacity(4096);
    let _ = writeln!(out, "{{");
    let _ = writeln!(out, "  \"ReverseProxy\": {{");

    // Clusters
    let _ = writeln!(out, "    \"Clusters\": {{");
    let _ = writeln!(out, "      \"legacy-cluster\": {{");
    let _ = writeln!(out, "        \"Destinations\": {{");
    let _ = writeln!(out, "          \"legacy\": {{");
    let _ = writeln!(out, "            \"Address\": \"{legacy_url}\"");
    let _ = writeln!(out, "          }}");
    let _ = writeln!(out, "        }}");
    let _ = writeln!(out, "      }},");
    let _ = writeln!(out, "      \"modern-cluster\": {{");
    let _ = writeln!(out, "        \"Destinations\": {{");
    let _ = writeln!(out, "          \"modern\": {{");
    let _ = writeln!(out, "            \"Address\": \"{modern_url}\"");
    let _ = writeln!(out, "          }}");
    let _ = writeln!(out, "        }}");
    let _ = writeln!(out, "      }}");
    let _ = writeln!(out, "    }},");

    // Routes
    let _ = writeln!(out, "    \"Routes\": {{");
    let total = pages.len();
    for (i, page) in pages.iter().enumerate() {
        let name = page_name_from_path(page);
        let route = route_from_path(page);
        let is_migrated = migrated.contains_key(page.as_str());
        let cluster = if is_migrated {
            "modern-cluster"
        } else {
            "legacy-cluster"
        };
        let comment = if is_migrated {
            " // MIGRATED"
        } else {
            " // LEGACY"
        };
        let trailing = if i + 1 < total { "," } else { "" };

        let _ = writeln!(out, "      \"route-{name}\": {{{comment}");
        let _ = writeln!(out, "        \"ClusterId\": \"{cluster}\",");
        let _ = writeln!(out, "        \"Match\": {{");
        let _ = writeln!(out, "          \"Path\": \"{route}\"");
        let _ = writeln!(out, "        }}");
        let _ = writeln!(out, "      }}{trailing}");
    }

    // Catch-all route to legacy
    if !pages.is_empty() {
        let _ = writeln!(out, "      ,\"route-catchall\": {{");
    } else {
        let _ = writeln!(out, "      \"route-catchall\": {{");
    }
    let _ = writeln!(out, "        \"ClusterId\": \"legacy-cluster\",");
    let _ = writeln!(out, "        \"Match\": {{");
    let _ = writeln!(out, "          \"Path\": \"{{**catch-all}}\"");
    let _ = writeln!(out, "        }}");
    let _ = writeln!(out, "      }}");

    let _ = writeln!(out, "    }}");
    let _ = writeln!(out, "  }}");
    let _ = writeln!(out, "}}");
    out
}

// ─── Feature flags ──────────────────────────────────────────────────────────

fn generate_feature_flags(pages: &[String], migrated: &BTreeMap<String, bool>) -> String {
    let mut out = String::with_capacity(8192);

    // Part 1: appsettings.FeatureFlags.json
    let _ = writeln!(out, "// ── appsettings.FeatureFlags.json ──");
    let _ = writeln!(out, "{{");
    let _ = writeln!(out, "  \"FeatureManagement\": {{");
    let total = pages.len();
    for (i, page) in pages.iter().enumerate() {
        let name = page_name_from_path(page);
        let enabled = migrated.contains_key(page.as_str());
        let trailing = if i + 1 < total { "," } else { "" };
        let _ = writeln!(out, "    \"Migration_{name}_Enabled\": {enabled}{trailing}");
    }
    let _ = writeln!(out, "  }}");
    let _ = writeln!(out, "}}");
    let _ = writeln!(out);

    // Part 2: FeatureFlagMiddleware.cs
    let _ = writeln!(out, "// ── FeatureFlagMiddleware.cs ──");
    let _ = writeln!(out, "using Microsoft.AspNetCore.Http;");
    let _ = writeln!(out, "using Microsoft.Extensions.Logging;");
    let _ = writeln!(out, "using Microsoft.FeatureManagement;");
    let _ = writeln!(out, "using System.Threading.Tasks;");
    let _ = writeln!(out);
    let _ = writeln!(out, "namespace Migration.Infrastructure;");
    let _ = writeln!(out);
    let _ = writeln!(out, "/// <summary>");
    let _ = writeln!(
        out,
        "/// Middleware that checks per-page feature flags and routes to the"
    );
    let _ = writeln!(
        out,
        "/// modern handler when the flag is enabled, otherwise falls through"
    );
    let _ = writeln!(out, "/// to the legacy reverse proxy.");
    let _ = writeln!(out, "/// </summary>");
    let _ = writeln!(out, "public class FeatureFlagMiddleware");
    let _ = writeln!(out, "{{");
    let _ = writeln!(out, "    private readonly RequestDelegate _next;");
    let _ = writeln!(out, "    private readonly IFeatureManager _featureManager;");
    let _ = writeln!(
        out,
        "    private readonly ILogger<FeatureFlagMiddleware> _logger;"
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "    public FeatureFlagMiddleware(");
    let _ = writeln!(out, "        RequestDelegate next,");
    let _ = writeln!(out, "        IFeatureManager featureManager,");
    let _ = writeln!(out, "        ILogger<FeatureFlagMiddleware> logger)");
    let _ = writeln!(out, "    {{");
    let _ = writeln!(out, "        _next = next;");
    let _ = writeln!(out, "        _featureManager = featureManager;");
    let _ = writeln!(out, "        _logger = logger;");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "    public async Task InvokeAsync(HttpContext context)"
    );
    let _ = writeln!(out, "    {{");
    let _ = writeln!(
        out,
        "        var path = context.Request.Path.Value ?? \"\";"
    );
    let _ = writeln!(out, "        var pageName = ExtractPageName(path);");
    let _ = writeln!(out);
    let _ = writeln!(out, "        if (!string.IsNullOrEmpty(pageName))");
    let _ = writeln!(out, "        {{");
    let _ = writeln!(
        out,
        "            var flagName = $\"Migration_{{pageName}}_Enabled\";"
    );
    let _ = writeln!(
        out,
        "            if (await _featureManager.IsEnabledAsync(flagName))"
    );
    let _ = writeln!(out, "            {{");
    let _ = writeln!(out, "                _logger.LogInformation(");
    let _ = writeln!(
        out,
        "                    \"Feature flag {{Flag}} enabled — routing {{Path}} to modern handler\","
    );
    let _ = writeln!(out, "                    flagName, path);");
    let _ = writeln!(
        out,
        "                context.Items[\"UseModernHandler\"] = true;"
    );
    let _ = writeln!(out, "            }}");
    let _ = writeln!(out, "            else");
    let _ = writeln!(out, "            {{");
    let _ = writeln!(out, "                _logger.LogDebug(");
    let _ = writeln!(
        out,
        "                    \"Feature flag {{Flag}} disabled — routing {{Path}} to legacy\","
    );
    let _ = writeln!(out, "                    flagName, path);");
    let _ = writeln!(out, "            }}");
    let _ = writeln!(out, "        }}");
    let _ = writeln!(out);
    let _ = writeln!(out, "        await _next(context);");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "    private static string? ExtractPageName(string path)"
    );
    let _ = writeln!(out, "    {{");
    let _ = writeln!(out, "        if (string.IsNullOrEmpty(path)) return null;");
    let _ = writeln!(
        out,
        "        var segments = path.TrimStart('/').Split('/');"
    );
    let _ = writeln!(out, "        var last = segments[^1];");
    let _ = writeln!(
        out,
        "        if (last.EndsWith(\".aspx\", System.StringComparison.OrdinalIgnoreCase))"
    );
    let _ = writeln!(out, "            return last[..^5]; // strip .aspx");
    let _ = writeln!(out, "        return last.Length > 0 ? last : null;");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out, "}}");

    out
}

// ─── Routing middleware ─────────────────────────────────────────────────────

fn generate_routing_middleware(
    pages: &[String],
    migrated: &BTreeMap<String, bool>,
    legacy_base_url: &str,
) -> String {
    let mut out = String::with_capacity(8192);

    let _ = writeln!(out, "using Microsoft.AspNetCore.Http;");
    let _ = writeln!(out, "using Microsoft.Extensions.Configuration;");
    let _ = writeln!(out, "using Microsoft.Extensions.Logging;");
    let _ = writeln!(out, "using System;");
    let _ = writeln!(out, "using System.Collections.Generic;");
    let _ = writeln!(out, "using System.Net.Http;");
    let _ = writeln!(out, "using System.Threading.Tasks;");
    let _ = writeln!(out);
    let _ = writeln!(out, "namespace Migration.Infrastructure;");
    let _ = writeln!(out);
    let _ = writeln!(out, "/// <summary>");
    let _ = writeln!(
        out,
        "/// Strangler fig routing middleware that checks whether an incoming"
    );
    let _ = writeln!(
        out,
        "/// request matches a migrated page and routes accordingly."
    );
    let _ = writeln!(out, "///");
    let _ = writeln!(
        out,
        "/// Supports percentage-based gradual rollout per page via configuration:"
    );
    let _ = writeln!(out, "///   StranglerFig:Rollout:{{PageName}} = 0..100");
    let _ = writeln!(out, "/// </summary>");
    let _ = writeln!(out, "public class StranglerFigMiddleware");
    let _ = writeln!(out, "{{");
    let _ = writeln!(out, "    private readonly RequestDelegate _next;");
    let _ = writeln!(
        out,
        "    private readonly IHttpClientFactory _httpClientFactory;"
    );
    let _ = writeln!(out, "    private readonly IConfiguration _configuration;");
    let _ = writeln!(
        out,
        "    private readonly ILogger<StranglerFigMiddleware> _logger;"
    );
    let _ = writeln!(out, "    private readonly HashSet<string> _migratedPages;");
    let _ = writeln!(out, "    private readonly string _legacyBaseUrl;");
    let _ = writeln!(out);
    let _ = writeln!(out, "    public StranglerFigMiddleware(");
    let _ = writeln!(out, "        RequestDelegate next,");
    let _ = writeln!(out, "        IHttpClientFactory httpClientFactory,");
    let _ = writeln!(out, "        IConfiguration configuration,");
    let _ = writeln!(out, "        ILogger<StranglerFigMiddleware> logger)");
    let _ = writeln!(out, "    {{");
    let _ = writeln!(out, "        _next = next;");
    let _ = writeln!(out, "        _httpClientFactory = httpClientFactory;");
    let _ = writeln!(out, "        _configuration = configuration;");
    let _ = writeln!(out, "        _logger = logger;");
    let _ = writeln!(
        out,
        "        _legacyBaseUrl = configuration[\"StranglerFig:LegacyBaseUrl\"]"
    );
    let _ = writeln!(out, "            ?? \"{legacy_base_url}\";");
    let _ = writeln!(out);

    // Emit the known migrated pages as a static set
    let _ = writeln!(
        out,
        "        _migratedPages = new HashSet<string>(StringComparer.OrdinalIgnoreCase)"
    );
    let _ = writeln!(out, "        {{");
    for page in pages {
        if migrated.contains_key(page.as_str()) {
            let name = page_name_from_path(page);
            let _ = writeln!(out, "            \"{name}\",");
        }
    }
    let _ = writeln!(out, "        }};");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "    public async Task InvokeAsync(HttpContext context)"
    );
    let _ = writeln!(out, "    {{");
    let _ = writeln!(
        out,
        "        var path = context.Request.Path.Value ?? \"\";"
    );
    let _ = writeln!(out, "        var pageName = ExtractPageName(path);");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "        if (pageName != null && _migratedPages.Contains(pageName))"
    );
    let _ = writeln!(out, "        {{");
    let _ = writeln!(
        out,
        "            // Check percentage-based rollout with sticky session affinity"
    );
    let _ = writeln!(
        out,
        "            var rolloutKey = $\"StranglerFig:Rollout:{{pageName}}\";"
    );
    let _ = writeln!(
        out,
        "            var rolloutPct = _configuration.GetValue<int>(rolloutKey, 100);"
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "            if (ShouldRouteToModern(context, pageName, rolloutPct))"
    );
    let _ = writeln!(out, "            {{");
    let _ = writeln!(out, "                _logger.LogInformation(");
    let _ = writeln!(
        out,
        "                    \"Strangler fig: routing {{Path}} to MODERN handler (rollout {{Pct}}%)\","
    );
    let _ = writeln!(out, "                    path, rolloutPct);");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "                // Let the request continue to the modern ASP.NET Core pipeline"
    );
    let _ = writeln!(out, "                await _next(context);");
    let _ = writeln!(out, "                return;");
    let _ = writeln!(out, "            }}");
    let _ = writeln!(out);
    let _ = writeln!(out, "            _logger.LogInformation(");
    let _ = writeln!(
        out,
        "                \"Strangler fig: routing {{Path}} to LEGACY (rollout {{Pct}}%, not selected)\","
    );
    let _ = writeln!(out, "                path, rolloutPct);");
    let _ = writeln!(out, "        }}");
    let _ = writeln!(out, "        else if (pageName != null)");
    let _ = writeln!(out, "        {{");
    let _ = writeln!(out, "            _logger.LogDebug(");
    let _ = writeln!(
        out,
        "                \"Strangler fig: {{Path}} is unmigrated — forwarding to legacy\","
    );
    let _ = writeln!(out, "                path);");
    let _ = writeln!(out, "        }}");
    let _ = writeln!(out);
    let _ = writeln!(out, "        // Forward to legacy via reverse proxy");
    let _ = writeln!(out, "        await ProxyToLegacy(context, path);");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out);

    // Sticky-session-aware percentage-based routing
    let _ = writeln!(out, "    /// <summary>");
    let _ = writeln!(
        out,
        "    /// Determines whether to route to modern, using sticky session affinity."
    );
    let _ = writeln!(
        out,
        "    /// Once a user is assigned to modern or legacy for a page during a session,"
    );
    let _ = writeln!(
        out,
        "    /// they stay on that backend for the duration (prevents mid-session flips)."
    );
    let _ = writeln!(out, "    /// </summary>");
    let _ = writeln!(
        out,
        "    private static bool ShouldRouteToModern(HttpContext context, string pageName, int rolloutPercentage)"
    );
    let _ = writeln!(out, "    {{");
    let _ = writeln!(out, "        if (rolloutPercentage >= 100) return true;");
    let _ = writeln!(out, "        if (rolloutPercentage <= 0) return false;");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "        // Sticky session: check if user already assigned for this page"
    );
    let _ = writeln!(
        out,
        "        var stickyKey = $\"StranglerFig_{{pageName}}\";"
    );
    let _ = writeln!(
        out,
        "        var sessionValue = context.Session.GetString(stickyKey);"
    );
    let _ = writeln!(out, "        if (sessionValue != null)");
    let _ = writeln!(out, "        {{");
    let _ = writeln!(out, "            return sessionValue == \"modern\";");
    let _ = writeln!(out, "        }}");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "        // First visit: roll the dice and persist the decision"
    );
    let _ = writeln!(
        out,
        "        var useModern = Random.Shared.Next(100) < rolloutPercentage;"
    );
    let _ = writeln!(
        out,
        "        context.Session.SetString(stickyKey, useModern ? \"modern\" : \"legacy\");"
    );
    let _ = writeln!(out, "        return useModern;");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out);

    // Proxy helper with correlation ID forwarding
    let _ = writeln!(
        out,
        "    private async Task ProxyToLegacy(HttpContext context, string path)"
    );
    let _ = writeln!(out, "    {{");
    let _ = writeln!(
        out,
        "        var client = _httpClientFactory.CreateClient(\"LegacyProxy\");"
    );
    let _ = writeln!(
        out,
        "        var targetUri = new Uri($\"{{_legacyBaseUrl}}{{path}}{{context.Request.QueryString}}\");"
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "        var requestMessage = new HttpRequestMessage");
    let _ = writeln!(out, "        {{");
    let _ = writeln!(
        out,
        "            Method = new HttpMethod(context.Request.Method),"
    );
    let _ = writeln!(out, "            RequestUri = targetUri");
    let _ = writeln!(out, "        }};");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "        // Copy relevant headers (skip Host to avoid routing issues)"
    );
    let _ = writeln!(
        out,
        "        foreach (var header in context.Request.Headers)"
    );
    let _ = writeln!(out, "        {{");
    let _ = writeln!(
        out,
        "            if (!header.Key.Equals(\"Host\", StringComparison.OrdinalIgnoreCase))"
    );
    let _ = writeln!(
        out,
        "                requestMessage.Headers.TryAddWithoutValidation(header.Key, header.Value.ToArray());"
    );
    let _ = writeln!(out, "        }}");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "        // Ensure correlation ID is forwarded for cross-boundary tracing"
    );
    let _ = writeln!(
        out,
        "        if (context.Request.Headers.TryGetValue(\"X-Correlation-Id\", out var correlationId))"
    );
    let _ = writeln!(out, "        {{");
    let _ = writeln!(
        out,
        "            requestMessage.Headers.TryAddWithoutValidation(\"X-Correlation-Id\", correlationId.ToString());"
    );
    let _ = writeln!(out, "        }}");
    let _ = writeln!(
        out,
        "        // Tag the request as proxied from the strangler fig layer"
    );
    let _ = writeln!(
        out,
        "        requestMessage.Headers.TryAddWithoutValidation(\"X-Forwarded-By\", \"StranglerFig\");"
    );
    let _ = writeln!(
        out,
        "        requestMessage.Headers.TryAddWithoutValidation(\"X-Original-Path\", path);"
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "        if (context.Request.ContentLength > 0)");
    let _ = writeln!(out, "        {{");
    let _ = writeln!(
        out,
        "            requestMessage.Content = new StreamContent(context.Request.Body);"
    );
    let _ = writeln!(out, "        }}");
    let _ = writeln!(out);
    let _ = writeln!(out, "        try");
    let _ = writeln!(out, "        {{");
    let _ = writeln!(
        out,
        "            var response = await client.SendAsync(requestMessage);"
    );
    let _ = writeln!(
        out,
        "            context.Response.StatusCode = (int)response.StatusCode;"
    );
    let _ = writeln!(out, "            foreach (var header in response.Headers)");
    let _ = writeln!(
        out,
        "                context.Response.Headers[header.Key] = header.Value.ToArray();"
    );
    let _ = writeln!(
        out,
        "            foreach (var header in response.Content.Headers)"
    );
    let _ = writeln!(
        out,
        "                context.Response.Headers[header.Key] = header.Value.ToArray();"
    );
    let _ = writeln!(
        out,
        "            context.Response.Headers.Remove(\"transfer-encoding\");"
    );
    let _ = writeln!(
        out,
        "            await response.Content.CopyToAsync(context.Response.Body);"
    );
    let _ = writeln!(out, "        }}");
    let _ = writeln!(out, "        catch (HttpRequestException ex)");
    let _ = writeln!(out, "        {{");
    let _ = writeln!(
        out,
        "            _logger.LogError(ex, \"Failed to proxy {{Path}} to legacy at {{Url}}\", path, targetUri);"
    );
    let _ = writeln!(out, "            context.Response.StatusCode = 502;");
    let _ = writeln!(
        out,
        "            await context.Response.WriteAsync(\"Legacy backend unavailable\");"
    );
    let _ = writeln!(out, "        }}");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out);

    // Page name extraction helper
    let _ = writeln!(
        out,
        "    private static string? ExtractPageName(string path)"
    );
    let _ = writeln!(out, "    {{");
    let _ = writeln!(out, "        if (string.IsNullOrEmpty(path)) return null;");
    let _ = writeln!(
        out,
        "        var segments = path.TrimStart('/').Split('/');"
    );
    let _ = writeln!(out, "        var last = segments[^1];");
    let _ = writeln!(
        out,
        "        if (last.EndsWith(\".aspx\", StringComparison.OrdinalIgnoreCase))"
    );
    let _ = writeln!(out, "            return last[..^5];");
    let _ = writeln!(out, "        return last.Length > 0 ? last : null;");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out, "}}");

    out
}

// ─── Health check ───────────────────────────────────────────────────────────

fn generate_health_check(pages: &[String], migrated: &BTreeMap<String, bool>) -> String {
    let total = pages.len();
    let migrated_count = pages
        .iter()
        .filter(|p| migrated.contains_key(p.as_str()))
        .count();

    let mut out = String::with_capacity(4096);

    let _ = writeln!(out, "using Microsoft.AspNetCore.Http;");
    let _ = writeln!(out, "using Microsoft.Extensions.Diagnostics.HealthChecks;");
    let _ = writeln!(out, "using Microsoft.FeatureManagement;");
    let _ = writeln!(out, "using System.Collections.Generic;");
    let _ = writeln!(out, "using System.Text.Json;");
    let _ = writeln!(out, "using System.Threading;");
    let _ = writeln!(out, "using System.Threading.Tasks;");
    let _ = writeln!(out);
    let _ = writeln!(out, "namespace Migration.Infrastructure;");
    let _ = writeln!(out);
    let _ = writeln!(out, "/// <summary>");
    let _ = writeln!(
        out,
        "/// Health check that reports the current migration progress, feature flag"
    );
    let _ = writeln!(out, "/// status, and legacy/modern traffic split.");
    let _ = writeln!(out, "/// </summary>");
    let _ = writeln!(out, "public class MigrationHealthCheck : IHealthCheck");
    let _ = writeln!(out, "{{");
    let _ = writeln!(out, "    private readonly IFeatureManager _featureManager;");
    let _ = writeln!(out);
    let _ = writeln!(out, "    // Static page inventory generated at build time");
    let _ = writeln!(out, "    private static readonly string[] AllPages = new[]");
    let _ = writeln!(out, "    {{");
    for page in pages {
        let name = page_name_from_path(page);
        let _ = writeln!(out, "        \"{name}\",");
    }
    let _ = writeln!(out, "    }};");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "    public MigrationHealthCheck(IFeatureManager featureManager)"
    );
    let _ = writeln!(out, "    {{");
    let _ = writeln!(out, "        _featureManager = featureManager;");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "    public async Task<HealthCheckResult> CheckHealthAsync("
    );
    let _ = writeln!(out, "        HealthCheckContext context,");
    let _ = writeln!(
        out,
        "        CancellationToken cancellationToken = default)"
    );
    let _ = writeln!(out, "    {{");
    let _ = writeln!(
        out,
        "        var flagStatus = new Dictionary<string, bool>();"
    );
    let _ = writeln!(out, "        var enabledCount = 0;");
    let _ = writeln!(out);
    let _ = writeln!(out, "        foreach (var page in AllPages)");
    let _ = writeln!(out, "        {{");
    let _ = writeln!(
        out,
        "            var flag = $\"Migration_{{page}}_Enabled\";"
    );
    let _ = writeln!(
        out,
        "            var enabled = await _featureManager.IsEnabledAsync(flag);"
    );
    let _ = writeln!(out, "            flagStatus[page] = enabled;");
    let _ = writeln!(out, "            if (enabled) enabledCount++;");
    let _ = writeln!(out, "        }}");
    let _ = writeln!(out);
    let _ = writeln!(out, "        var totalPages = AllPages.Length;");
    let _ = writeln!(out, "        var progressPct = totalPages > 0");
    let _ = writeln!(
        out,
        "            ? (double)enabledCount / totalPages * 100.0"
    );
    let _ = writeln!(out, "            : 0.0;");
    let _ = writeln!(out);
    let _ = writeln!(out, "        var data = new Dictionary<string, object>");
    let _ = writeln!(out, "        {{");
    let _ = writeln!(out, "            [\"total_pages\"] = totalPages,");
    let _ = writeln!(out, "            [\"migrated_pages\"] = enabledCount,");
    let _ = writeln!(
        out,
        "            [\"unmigrated_pages\"] = totalPages - enabledCount,"
    );
    let _ = writeln!(
        out,
        "            [\"progress_percent\"] = System.Math.Round(progressPct, 1),"
    );
    let _ = writeln!(out, "            [\"feature_flags\"] = flagStatus,");
    let _ = writeln!(
        out,
        "            [\"initial_migrated_count\"] = {migrated_count},"
    );
    let _ = writeln!(out, "            [\"initial_total_count\"] = {total},");
    let _ = writeln!(out, "        }};");
    let _ = writeln!(out);
    let _ = writeln!(out, "        var status = enabledCount == totalPages");
    let _ = writeln!(
        out,
        "            ? HealthStatus.Healthy     // all pages migrated"
    );
    let _ = writeln!(out, "            : enabledCount > 0");
    let _ = writeln!(
        out,
        "                ? HealthStatus.Degraded  // partial migration"
    );
    let _ = writeln!(
        out,
        "                : HealthStatus.Unhealthy; // no pages migrated yet"
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "        return new HealthCheckResult(status,");
    let _ = writeln!(
        out,
        "            description: $\"Migration progress: {{enabledCount}}/{{totalPages}} pages ({{progressPct:F1}}%)\","
    );
    let _ = writeln!(out, "            data: data);");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out, "}}");

    out
}

// ─── Program.cs registration ────────────────────────────────────────────────

fn generate_program_cs(legacy_url: &str, _modern_url: &str) -> String {
    let mut out = String::with_capacity(4096);

    let _ = writeln!(
        out,
        "// ── Program.cs — Strangler Fig Infrastructure Registration ──"
    );
    let _ = writeln!(out, "//");
    let _ = writeln!(
        out,
        "// Add this code to your ASP.NET Core Program.cs to wire up the"
    );
    let _ = writeln!(out, "// strangler fig migration infrastructure.");
    let _ = writeln!(out);
    let _ = writeln!(out, "using Migration.Infrastructure;");
    let _ = writeln!(out, "using Microsoft.FeatureManagement;");
    let _ = writeln!(out, "using Polly;");
    let _ = writeln!(out, "using Polly.Extensions.Http;");
    let _ = writeln!(out);
    let _ = writeln!(out, "var builder = WebApplication.CreateBuilder(args);");
    let _ = writeln!(out);
    let _ = writeln!(out, "// ── Configuration sources ──");
    let _ = writeln!(
        out,
        "builder.Configuration.AddJsonFile(\"appsettings.YARP.json\", optional: false);"
    );
    let _ = writeln!(
        out,
        "builder.Configuration.AddJsonFile(\"appsettings.FeatureFlags.json\", optional: false);"
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "// ── YARP reverse proxy ──");
    let _ = writeln!(out, "builder.Services.AddReverseProxy()");
    let _ = writeln!(
        out,
        "    .LoadFromConfig(builder.Configuration.GetSection(\"ReverseProxy\"));"
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "// ── Feature management (per-page migration toggles) ──"
    );
    let _ = writeln!(out, "builder.Services.AddFeatureManagement();");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "// ── HttpClient for legacy proxy with circuit breaker + retry ──"
    );
    let _ = writeln!(
        out,
        "builder.Services.AddHttpClient(\"LegacyProxy\", client =>"
    );
    let _ = writeln!(out, "{{");
    let _ = writeln!(out, "    client.BaseAddress = new Uri(\"{legacy_url}\");");
    let _ = writeln!(out, "    client.Timeout = TimeSpan.FromSeconds(30);");
    let _ = writeln!(
        out,
        "    client.DefaultRequestHeaders.Add(\"X-Forwarded-By\", \"StranglerFig\");"
    );
    let _ = writeln!(out, "}})");
    let _ = writeln!(out, ".AddPolicyHandler(GetRetryPolicy())");
    let _ = writeln!(out, ".AddPolicyHandler(GetCircuitBreakerPolicy());");
    let _ = writeln!(out);
    let _ = writeln!(out, "// ── Health checks ──");
    let _ = writeln!(out, "builder.Services.AddHealthChecks()");
    let _ = writeln!(
        out,
        "    .AddCheck<MigrationHealthCheck>(\"migration-progress\");"
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "// ── Distributed cache for sticky session affinity ──"
    );
    let _ = writeln!(out, "builder.Services.AddDistributedMemoryCache();");
    let _ = writeln!(out, "builder.Services.AddSession(options =>");
    let _ = writeln!(out, "{{");
    let _ = writeln!(out, "    options.IdleTimeout = TimeSpan.FromMinutes(30);");
    let _ = writeln!(out, "    options.Cookie.HttpOnly = true;");
    let _ = writeln!(out, "    options.Cookie.IsEssential = true;");
    let _ = writeln!(out, "}});");
    let _ = writeln!(out);
    let _ = writeln!(out, "var app = builder.Build();");
    let _ = writeln!(out);
    let _ = writeln!(out, "// ── Middleware pipeline (order matters!) ──");
    let _ = writeln!(out, "app.UseSession();");
    let _ = writeln!(out, "app.UseMiddleware<CorrelationIdMiddleware>();");
    let _ = writeln!(out, "app.UseMiddleware<FeatureFlagMiddleware>();");
    let _ = writeln!(out, "app.UseMiddleware<StranglerFigMiddleware>();");
    let _ = writeln!(out, "app.MapReverseProxy();");
    let _ = writeln!(
        out,
        "app.MapHealthChecks(\"/health/migration\", new Microsoft.AspNetCore.Diagnostics.HealthChecks.HealthCheckOptions"
    );
    let _ = writeln!(out, "{{");
    let _ = writeln!(out, "    ResponseWriter = async (context, report) =>");
    let _ = writeln!(out, "    {{");
    let _ = writeln!(
        out,
        "        context.Response.ContentType = \"application/json\";"
    );
    let _ = writeln!(
        out,
        "        var json = System.Text.Json.JsonSerializer.Serialize(new"
    );
    let _ = writeln!(out, "        {{");
    let _ = writeln!(out, "            status = report.Status.ToString(),");
    let _ = writeln!(out, "            entries = report.Entries.ToDictionary(");
    let _ = writeln!(out, "                e => e.Key,");
    let _ = writeln!(
        out,
        "                e => new {{ status = e.Value.Status.ToString(), data = e.Value.Data }})"
    );
    let _ = writeln!(out, "        }});");
    let _ = writeln!(out, "        await context.Response.WriteAsync(json);");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out, "}});");
    let _ = writeln!(out);
    let _ = writeln!(out, "app.Run();");
    let _ = writeln!(out);
    let _ = writeln!(out, "// ── Polly resilience policies ──");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "static IAsyncPolicy<HttpResponseMessage> GetRetryPolicy() =>"
    );
    let _ = writeln!(out, "    HttpPolicyExtensions");
    let _ = writeln!(out, "        .HandleTransientHttpError()");
    let _ = writeln!(out, "        .WaitAndRetryAsync(3, retryAttempt =>");
    let _ = writeln!(
        out,
        "            TimeSpan.FromSeconds(Math.Pow(2, retryAttempt)));"
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "static IAsyncPolicy<HttpResponseMessage> GetCircuitBreakerPolicy() =>"
    );
    let _ = writeln!(out, "    HttpPolicyExtensions");
    let _ = writeln!(out, "        .HandleTransientHttpError()");
    let _ = writeln!(out, "        .CircuitBreakerAsync(");
    let _ = writeln!(out, "            handledEventsAllowedBeforeBreaking: 5,");
    let _ = writeln!(
        out,
        "            durationOfBreak: TimeSpan.FromSeconds(30));"
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "// ── Correlation ID Middleware ──");
    let _ = writeln!(out);
    let _ = writeln!(out, "public class CorrelationIdMiddleware");
    let _ = writeln!(out, "{{");
    let _ = writeln!(out, "    private readonly RequestDelegate _next;");
    let _ = writeln!(
        out,
        "    private const string CorrelationHeader = \"X-Correlation-Id\";"
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "    public CorrelationIdMiddleware(RequestDelegate next) => _next = next;"
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "    public async Task InvokeAsync(HttpContext context)"
    );
    let _ = writeln!(out, "    {{");
    let _ = writeln!(
        out,
        "        if (!context.Request.Headers.ContainsKey(CorrelationHeader))"
    );
    let _ = writeln!(out, "        {{");
    let _ = writeln!(
        out,
        "            context.Request.Headers[CorrelationHeader] = Guid.NewGuid().ToString();"
    );
    let _ = writeln!(out, "        }}");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "        var correlationId = context.Request.Headers[CorrelationHeader].ToString();"
    );
    let _ = writeln!(out, "        context.Response.OnStarting(() =>");
    let _ = writeln!(out, "        {{");
    let _ = writeln!(
        out,
        "            context.Response.Headers[CorrelationHeader] = correlationId;"
    );
    let _ = writeln!(out, "            return Task.CompletedTask;");
    let _ = writeln!(out, "        }});");
    let _ = writeln!(out);
    let _ = writeln!(out, "        using var scope = context.RequestServices");
    let _ = writeln!(out, "            .GetRequiredService<ILoggerFactory>()");
    let _ = writeln!(out, "            .CreateLogger<CorrelationIdMiddleware>()");
    let _ = writeln!(
        out,
        "            .BeginScope(new Dictionary<string, object>"
    );
    let _ = writeln!(out, "            {{");
    let _ = writeln!(out, "                [\"CorrelationId\"] = correlationId");
    let _ = writeln!(out, "            }});");
    let _ = writeln!(out);
    let _ = writeln!(out, "        await _next(context);");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out, "}}");

    out
}

// ─── Deployment steps ───────────────────────────────────────────────────────

fn build_deployment_steps(legacy_url: &str, modern_url: &str) -> Vec<String> {
    vec![
        "1. Install NuGet packages:\n   - Yarp.ReverseProxy\n   - Microsoft.FeatureManagement.AspNetCore\n   - Microsoft.Extensions.Diagnostics.HealthChecks\n   - Microsoft.Extensions.Http.Polly\n   - Polly.Extensions.Http".into(),
        format!("2. Deploy the legacy application at {legacy_url}"),
        format!("3. Deploy the modern ASP.NET Core application at {modern_url}"),
        "4. Copy the generated config files to the modern app's root:\n   - appsettings.YARP.json (YARP reverse proxy routes)\n   - appsettings.FeatureFlags.json (per-page migration toggles)".into(),
        "5. Copy the generated C# files to Migration/Infrastructure/:\n   - FeatureFlagMiddleware.cs\n   - StranglerFigMiddleware.cs\n   - MigrationHealthCheck.cs\n   - CorrelationIdMiddleware.cs (from Program.cs output)".into(),
        "6. Register services and middleware in Program.cs (see generated Program.cs output for complete registration code with Polly resilience, session affinity, and health check JSON writer)".into(),
        "7. Enable migration for a page by setting its feature flag to true:\n   Set \"Migration_{PageName}_Enabled\": true in appsettings.FeatureFlags.json".into(),
        "8. For gradual rollout, set a percentage (0-100) in appsettings.json:\n   \"StranglerFig:Rollout:{PageName}\": 25  (routes 25% of traffic to modern)\n   Sticky sessions ensure each user stays on their assigned backend for the session duration".into(),
        "9. Monitor the health endpoint: GET /health/migration\n   Returns JSON with per-page flag status, total/migrated counts, and progress percentage".into(),
        "10. Verify cross-boundary tracing: X-Correlation-Id headers flow between modern and legacy layers".into(),
        "11. Once all pages show 100% modern traffic with stable health checks, decommission the legacy application and remove the strangler fig middleware layer".into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_name_from_path_strips_extension() {
        assert_eq!(page_name_from_path("Orders.aspx"), "Orders");
        assert_eq!(page_name_from_path("Admin/Users.aspx"), "Users");
        assert_eq!(
            page_name_from_path("App\\Pages\\Dashboard.aspx"),
            "Dashboard"
        );
    }

    #[test]
    fn route_from_path_adds_leading_slash() {
        assert_eq!(route_from_path("Orders.aspx"), "/Orders.aspx");
        assert_eq!(route_from_path("/Admin/Users.aspx"), "/Admin/Users.aspx");
        // Backslashes normalized to forward slashes for URL routes
        assert_eq!(
            route_from_path("App\\Pages\\Dashboard.aspx"),
            "/App/Pages/Dashboard.aspx"
        );
    }

    #[test]
    fn yarp_config_contains_clusters() {
        let pages = vec!["Home.aspx".to_string(), "About.aspx".to_string()];
        let migrated = BTreeMap::new();
        let config = generate_yarp_config(
            &pages,
            &migrated,
            "http://legacy:5000",
            "http://modern:5001",
        );
        assert!(config.contains("legacy-cluster"));
        assert!(config.contains("modern-cluster"));
        assert!(config.contains("http://legacy:5000"));
        assert!(config.contains("http://modern:5001"));
        assert!(config.contains("route-Home"));
        assert!(config.contains("route-About"));
        assert!(config.contains("route-catchall"));
    }

    #[test]
    fn yarp_config_routes_migrated_to_modern() {
        let pages = vec!["Home.aspx".to_string(), "Orders.aspx".to_string()];
        let mut migrated = BTreeMap::new();
        migrated.insert("Home.aspx".to_string(), true);

        let config = generate_yarp_config(&pages, &migrated, "http://l", "http://m");
        // Home should be routed to modern-cluster
        let home_section = config
            .find("route-Home")
            .and_then(|start| config[start..].find("ClusterId").map(|off| start + off))
            .map(|pos| &config[pos..pos + 40])
            .unwrap_or("");
        assert!(home_section.contains("modern-cluster"));

        // Orders should be routed to legacy-cluster
        let orders_section = config
            .find("route-Orders")
            .and_then(|start| config[start..].find("ClusterId").map(|off| start + off))
            .map(|pos| &config[pos..pos + 40])
            .unwrap_or("");
        assert!(orders_section.contains("legacy-cluster"));
    }

    #[test]
    fn feature_flags_defaults_to_false() {
        let pages = vec!["Home.aspx".to_string(), "Orders.aspx".to_string()];
        let migrated = BTreeMap::new();
        let config = generate_feature_flags(&pages, &migrated);
        assert!(config.contains("\"Migration_Home_Enabled\": false"));
        assert!(config.contains("\"Migration_Orders_Enabled\": false"));
    }

    #[test]
    fn feature_flags_migrated_is_true() {
        let pages = vec!["Home.aspx".to_string()];
        let mut migrated = BTreeMap::new();
        migrated.insert("Home.aspx".to_string(), true);

        let config = generate_feature_flags(&pages, &migrated);
        assert!(config.contains("\"Migration_Home_Enabled\": true"));
    }

    #[test]
    fn feature_flags_contains_middleware_class() {
        let pages = vec!["Home.aspx".to_string()];
        let config = generate_feature_flags(&pages, &BTreeMap::new());
        assert!(config.contains("public class FeatureFlagMiddleware"));
        assert!(config.contains("IFeatureManager"));
        assert!(config.contains("InvokeAsync"));
        assert!(config.contains("Migration.Infrastructure"));
    }

    #[test]
    fn routing_middleware_contains_class() {
        let pages = vec!["Home.aspx".to_string()];
        let migrated = BTreeMap::new();
        let code = generate_routing_middleware(&pages, &migrated, "http://localhost:5000");
        assert!(code.contains("public class StranglerFigMiddleware"));
        assert!(code.contains("ShouldRouteToModern"));
        assert!(code.contains("ProxyToLegacy"));
        assert!(code.contains("rolloutPercentage"));
        assert!(code.contains("http://localhost:5000"));
    }

    #[test]
    fn routing_middleware_includes_migrated_pages() {
        let pages = vec!["Home.aspx".to_string(), "Orders.aspx".to_string()];
        let mut migrated = BTreeMap::new();
        migrated.insert("Home.aspx".to_string(), true);

        let code = generate_routing_middleware(&pages, &migrated, "http://l:5000");
        // Home should be in the migrated set
        assert!(code.contains("\"Home\""));
        // Orders should NOT be in the migrated set
        assert!(!code.contains("\"Orders\""));
    }

    #[test]
    fn health_check_contains_all_pages() {
        let pages = vec![
            "Home.aspx".to_string(),
            "Orders.aspx".to_string(),
            "Dashboard.aspx".to_string(),
        ];
        let migrated = BTreeMap::new();
        let code = generate_health_check(&pages, &migrated);

        assert!(code.contains("public class MigrationHealthCheck"));
        assert!(code.contains("IHealthCheck"));
        assert!(code.contains("\"Home\""));
        assert!(code.contains("\"Orders\""));
        assert!(code.contains("\"Dashboard\""));
        assert!(code.contains("progress_percent"));
        assert!(code.contains("initial_migrated_count"));
    }

    #[test]
    fn health_check_reports_initial_counts() {
        let pages = vec!["Home.aspx".to_string(), "Orders.aspx".to_string()];
        let mut migrated = BTreeMap::new();
        migrated.insert("Home.aspx".to_string(), true);

        let code = generate_health_check(&pages, &migrated);
        assert!(code.contains("\"initial_migrated_count\"] = 1"));
        assert!(code.contains("\"initial_total_count\"] = 2"));
    }

    #[test]
    fn deployment_steps_complete() {
        let steps = build_deployment_steps("http://l:5000", "http://m:5001");
        assert!(steps.len() >= 10);
        assert!(steps[0].contains("NuGet"));
        assert!(steps[0].contains("Polly"));
        assert!(steps[1].contains("http://l:5000"));
        assert!(steps[2].contains("http://m:5001"));
        assert!(steps.iter().any(|s| s.contains("Program.cs")));
        assert!(steps.iter().any(|s| s.contains("/health/migration")));
        assert!(steps.iter().any(|s| s.contains("Sticky sessions")));
        assert!(steps.iter().any(|s| s.contains("X-Correlation-Id")));
    }

    #[test]
    fn empty_pages_generates_valid_config() {
        let pages: Vec<String> = vec![];
        let migrated = BTreeMap::new();

        let yarp = generate_yarp_config(&pages, &migrated, "http://l", "http://m");
        assert!(yarp.contains("route-catchall"));

        let flags = generate_feature_flags(&pages, &migrated);
        assert!(flags.contains("FeatureManagement"));

        let middleware = generate_routing_middleware(&pages, &migrated, "http://l");
        assert!(middleware.contains("StranglerFigMiddleware"));

        let health = generate_health_check(&pages, &migrated);
        assert!(health.contains("MigrationHealthCheck"));
    }

    #[test]
    fn program_cs_contains_full_registration() {
        let code = generate_program_cs("http://legacy:5000", "http://modern:5001");
        // YARP
        assert!(code.contains("AddReverseProxy"));
        assert!(code.contains("LoadFromConfig"));
        // Feature management
        assert!(code.contains("AddFeatureManagement"));
        // HttpClient with resilience
        assert!(code.contains("AddHttpClient"));
        assert!(code.contains("http://legacy:5000"));
        assert!(code.contains("GetRetryPolicy"));
        assert!(code.contains("GetCircuitBreakerPolicy"));
        // Health checks
        assert!(code.contains("AddHealthChecks"));
        assert!(code.contains("MigrationHealthCheck"));
        // Middleware order
        assert!(code.contains("UseSession"));
        assert!(code.contains("CorrelationIdMiddleware"));
        assert!(code.contains("FeatureFlagMiddleware"));
        assert!(code.contains("StranglerFigMiddleware"));
        assert!(code.contains("MapReverseProxy"));
        assert!(code.contains("MapHealthChecks"));
        // Polly policies
        assert!(code.contains("CircuitBreakerAsync"));
        assert!(code.contains("WaitAndRetryAsync"));
        // Correlation ID middleware class
        assert!(code.contains("public class CorrelationIdMiddleware"));
        assert!(code.contains("X-Correlation-Id"));
        assert!(code.contains("BeginScope"));
    }

    #[test]
    fn routing_middleware_has_sticky_sessions() {
        let pages = vec!["Home.aspx".to_string()];
        let mut migrated = BTreeMap::new();
        migrated.insert("Home.aspx".to_string(), true);
        let code = generate_routing_middleware(&pages, &migrated, "http://l");
        assert!(code.contains("Session.GetString"));
        assert!(code.contains("Session.SetString"));
        assert!(code.contains("StranglerFig_"));
        assert!(code.contains("sticky"));
    }

    #[test]
    fn routing_middleware_forwards_correlation_id() {
        let pages = vec!["Home.aspx".to_string()];
        let migrated = BTreeMap::new();
        let code = generate_routing_middleware(&pages, &migrated, "http://l");
        assert!(code.contains("X-Correlation-Id"));
        assert!(code.contains("X-Forwarded-By"));
        assert!(code.contains("X-Original-Path"));
    }

    #[test]
    fn program_cs_middleware_order_correct() {
        let code = generate_program_cs("http://l", "http://m");
        // Session must come before correlation and routing middleware
        let session_pos = code.find("UseSession").unwrap();
        let correlation_pos = code.find("CorrelationIdMiddleware").unwrap();
        let feature_pos = code.find("FeatureFlagMiddleware").unwrap();
        let strangler_pos = code
            .find("StranglerFigMiddleware")
            .expect("StranglerFigMiddleware in code");
        let proxy_pos = code.find("MapReverseProxy").unwrap();

        assert!(session_pos < correlation_pos);
        assert!(correlation_pos < feature_pos);
        assert!(feature_pos < strangler_pos);
        assert!(strangler_pos < proxy_pos);
    }
}
