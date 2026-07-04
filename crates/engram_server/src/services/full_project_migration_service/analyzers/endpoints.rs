//! Extracted analyzer: endpoints.
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

pub(crate) fn build_service_endpoint_summary(
    graph: &Arc<GraphStore>,
    project_id: &str,
) -> ServiceEndpointSummary {
    let ws = edges_or_warn(
        graph.list_edges_by_kind(project_id, EdgeKind::ExposesWebService, 1_000),
        "ExposesWebService",
    );
    let hh = edges_or_warn(
        graph.list_edges_by_kind(project_id, EdgeKind::ExposesHttpHandler, 1_000),
        "ExposesHttpHandler",
    );
    let wcf = edges_or_warn(
        graph.list_edges_by_kind(project_id, EdgeKind::ExposesWcfService, 1_000),
        "ExposesWcfService",
    );
    let mods = edges_or_warn(
        graph.list_edges_by_kind(project_id, EdgeKind::RegistersModule, 1_000),
        "RegistersModule",
    );
    let routes = edges_or_warn(
        graph.list_edges_by_kind(project_id, EdgeKind::RegistersHandler, 1_000),
        "RegistersHandler",
    );

    // Get ApiCall edges to cross-reference callers
    let api_calls = edges_or_warn(
        graph.list_edges_by_kind(project_id, EdgeKind::ApiCall, 10_000),
        "ApiCall",
    );

    let build_endpoints = |edges: &[engram_graph::Edge], modern: &str| -> Vec<ServiceEndpoint> {
        let mut map: BTreeMap<String, ServiceEndpoint> = BTreeMap::new();
        for e in edges {
            let file_path = super::common::extract_file_from_node_id(&e.source_id);
            let entry = map
                .entry(file_path.clone())
                .or_insert_with(|| ServiceEndpoint {
                    file_path: file_path.clone(),
                    service_name: e.target_id.clone(),
                    methods: vec![],
                    modern_equivalent: modern.to_string(),
                    called_by: vec![],
                });
            // Extract method name from metadata if available
            if let Some(ref meta) = e.metadata
                && let Some(method) = meta.get("method_name").and_then(|v| v.as_str())
                && !entry.methods.contains(&method.to_string())
            {
                entry.methods.push(method.to_string());
            }
        }
        // Cross-reference with ApiCall edges
        for ep in map.values_mut() {
            for ac in &api_calls {
                let target_file = super::common::extract_file_from_node_id(&ac.target_id);
                if target_file == ep.file_path || ac.target_id.contains(&ep.service_name) {
                    let caller = super::common::extract_file_from_node_id(&ac.source_id);
                    if !ep.called_by.contains(&caller) {
                        ep.called_by.push(caller);
                    }
                }
            }
        }
        map.into_values().collect()
    };

    let web_services = build_endpoints(&ws, "Minimal API / Web API controller");
    let http_handlers = build_endpoints(&hh, "Minimal API endpoint / Middleware");
    let wcf_services = build_endpoints(&wcf, "gRPC service or Web API controller");
    let http_modules: Vec<ServiceEndpoint> = mods
        .iter()
        .map(|e| ServiceEndpoint {
            file_path: super::common::extract_file_from_node_id(&e.source_id),
            service_name: e.target_id.clone(),
            methods: vec![],
            modern_equivalent: "ASP.NET Core Middleware".into(),
            called_by: vec![],
        })
        .collect();
    let route_handlers: Vec<ServiceEndpoint> = routes
        .iter()
        .map(|e| ServiceEndpoint {
            file_path: super::common::extract_file_from_node_id(&e.source_id),
            service_name: e.target_id.clone(),
            methods: vec![],
            modern_equivalent: "app.MapGet/MapPost route".into(),
            called_by: vec![],
        })
        .collect();

    let total = web_services.len()
        + http_handlers.len()
        + wcf_services.len()
        + http_modules.len()
        + route_handlers.len();

    ServiceEndpointSummary {
        web_services,
        http_handlers,
        wcf_services,
        http_modules,
        route_handlers,
        total_endpoints: total,
    }
}
