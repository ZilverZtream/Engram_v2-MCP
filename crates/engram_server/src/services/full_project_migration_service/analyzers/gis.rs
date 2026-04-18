//! Extracted analyzer: gis.
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


pub(crate) fn build_gis_analysis(
    graph: &Arc<GraphStore>,
    project_id: &str,
    target_stack: &str,
) -> GisAnalysisSummary {
    let spatial_edges = edges_or_warn(
        graph.list_edges_by_kind(project_id, EdgeKind::SpatialCall, 10_000),
        "SpatialCall",
    );

    // Query insight nodes for GIS inventories
    let gis_insights = nodes_or_warn(
        graph.query_nodes(project_id, Some("insight"), None, None, 1_000),
        "gis_insights",
    )
    .into_iter()
    .filter(|n| {
        let name_lower = n.name.to_lowercase();
        name_lower.contains("gis_inventory")
            || name_lower.contains("google_maps")
            || name_lower.contains("esri")
            || name_lower.contains("leaflet")
            || name_lower.contains("openlayers")
            || name_lower.contains("spatial")
    })
    .collect::<Vec<_>>();

    if spatial_edges.is_empty() && gis_insights.is_empty() {
        return GisAnalysisSummary {
            has_gis: false,
            libraries_detected: vec![],
            total_spatial_calls: 0,
            files_with_gis: vec![],
            migration_complexity: "none".into(),
            modern_targets: GisModernTargets {
                react: vec![],
                blazor: vec![],
                angular: vec![],
            },
        };
    }

    // Collect files with GIS
    let mut files_with_gis: Vec<String> = spatial_edges
        .iter()
        .map(|e| super::common::extract_file_from_node_id(&e.source_id))
        .collect();
    files_with_gis.sort();
    files_with_gis.dedup();

    // Build library summaries from insight metadata
    let mut libraries: BTreeMap<String, GisLibrarySummary> = BTreeMap::new();
    for insight in &gis_insights {
        if let Some(ref meta) = insight.metadata {
            let library = meta
                .get("library")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let entry = libraries
                .entry(library.clone())
                .or_insert_with(|| GisLibrarySummary {
                    library: library.clone(),
                    files: vec![],
                    class_count: 0,
                    features: vec![],
                    has_3d: false,
                    has_drawing: false,
                    has_geocoding: false,
                    has_clustering: false,
                    has_wms: false,
                    api_keys_detected: 0,
                    api_style: None,
                });

            let file = insight.file_path.as_str().to_string();
            if !entry.files.contains(&file) {
                entry.files.push(file);
            }

            if let Some(cc) = meta.get("class_count").and_then(|v| v.as_u64()) {
                entry.class_count = entry.class_count.max(cc as usize);
            }
            if meta
                .get("has_places_api")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
                && !entry.features.contains(&"Places API".to_string())
            {
                entry.features.push("Places API".into());
            }
            if meta
                .get("has_streetview")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
                && !entry.features.contains(&"StreetView".to_string())
            {
                entry.features.push("StreetView".into());
            }
            if meta
                .get("has_directions")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
                && !entry.features.contains(&"Directions".into())
            {
                entry.features.push("Directions".into());
            }
            if meta
                .get("has_heatmap")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
                && !entry.features.contains(&"Heatmap".into())
            {
                entry.features.push("Heatmap".into());
            }
            if meta
                .get("has_drawing")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                entry.has_drawing = true;
                if !entry.features.contains(&"Drawing tools".into()) {
                    entry.features.push("Drawing tools".into());
                }
            }
            if meta
                .get("has_geocoding")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                entry.has_geocoding = true;
                if !entry.features.contains(&"Geocoding".into()) {
                    entry.features.push("Geocoding".into());
                }
            }
            if meta
                .get("has_kml")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
                && !entry.features.contains(&"KML layers".into())
            {
                entry.features.push("KML layers".into());
            }
            if meta
                .get("has_3d")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                entry.has_3d = true;
            }
            if meta
                .get("has_clustering")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                entry.has_clustering = true;
            }
            if meta
                .get("has_wms")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                entry.has_wms = true;
            }
            if let Some(keys) = meta.get("api_keys_detected").and_then(|v| v.as_u64()) {
                entry.api_keys_detected = entry.api_keys_detected.max(keys as usize);
            }
            if let Some(style) = meta.get("api_style").and_then(|v| v.as_str()) {
                entry.api_style = Some(style.to_string());
            }
        }
    }

    // Also gather from spatial edges if insights are missing
    for edge in &spatial_edges {
        let meta = edge.metadata.as_ref();
        let library = meta
            .and_then(|m| m.get("library").and_then(|v| v.as_str()))
            .unwrap_or("unknown")
            .to_string();
        let entry = libraries
            .entry(library.clone())
            .or_insert_with(|| GisLibrarySummary {
                library: library.clone(),
                files: vec![],
                class_count: 0,
                features: vec![],
                has_3d: false,
                has_drawing: false,
                has_geocoding: false,
                has_clustering: false,
                has_wms: false,
                api_keys_detected: 0,
                api_style: None,
            });
        let file = super::common::extract_file_from_node_id(&edge.source_id);
        if !entry.files.contains(&file) {
            entry.files.push(file);
        }
    }

    let libraries_vec: Vec<GisLibrarySummary> = libraries.into_values().collect();

    // Determine overall complexity
    let max_complexity_insight = gis_insights
        .iter()
        .filter_map(|n| n.metadata.as_ref()?.get("migration_complexity")?.as_str())
        .max_by_key(|c| match *c {
            "high" => 3,
            "medium" => 2,
            _ => 1,
        })
        .unwrap_or("medium");
    let migration_complexity = if libraries_vec.len() > 1 || spatial_edges.len() > 20 {
        "high".to_string()
    } else {
        max_complexity_insight.to_string()
    };

    // Modern targets based on target_stack
    let modern_targets = build_gis_modern_targets(&libraries_vec, target_stack);

    GisAnalysisSummary {
        has_gis: true,
        libraries_detected: libraries_vec,
        total_spatial_calls: spatial_edges.len(),
        files_with_gis,
        migration_complexity,
        modern_targets,
    }
}

pub(crate) fn build_gis_modern_targets(
    libraries: &[GisLibrarySummary],
    target_stack: &str,
) -> GisModernTargets {
    let mut react = Vec::new();
    let mut blazor = Vec::new();
    let mut angular = Vec::new();

    for lib in libraries {
        match lib.library.to_lowercase().as_str() {
            "google_maps" | "google maps" => {
                react.push("@react-google-maps/api".into());
                blazor.push("BlazorGoogleMaps NuGet".into());
                angular.push("@angular/google-maps".into());
            }
            "leaflet" => {
                react.push("react-leaflet".into());
                blazor.push("BlazorLeaflet NuGet".into());
                angular.push("ngx-leaflet".into());
            }
            "openlayers" => {
                react.push("rlayers".into());
                blazor.push("OpenLayers JS interop".into());
                angular.push("ngx-openlayers".into());
            }
            "esri_arcgis" | "esri" | "arcgis" => {
                react.push("@arcgis/core + React wrapper".into());
                blazor.push("ArcGIS REST JS (@esri/arcgis-rest-request)".into());
                angular.push("@arcgis/core + Angular wrapper".into());
            }
            _ => {}
        }
    }

    // Highlight the one matching target_stack
    let ts = target_stack.to_lowercase();
    if ts.contains("blazor") {
        react.clear();
        angular.clear();
    } else if ts.contains("react") {
        blazor.clear();
        angular.clear();
    } else if ts.contains("angular") {
        react.clear();
        blazor.clear();
    }

    GisModernTargets {
        react,
        blazor,
        angular,
    }
}
