//! Extracted analyzer: anti patterns.
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

pub(crate) fn build_anti_pattern_summary(
    graph: &Arc<GraphStore>,
    project_id: &str,
) -> AntiPatternSummary {
    let detected =
        pattern_detection_service::detect_design_antipatterns(graph, project_id, 15, 5, 4)
            .unwrap_or_else(|e| {
                tracing::warn!("anti-pattern detection failed: {e}");
                vec![]
            });

    let mut by_type: BTreeMap<String, usize> = BTreeMap::new();
    let mut critical_items = Vec::new();
    let mut migration_impact = Vec::new();

    for ap in &detected {
        *by_type.entry(ap.pattern_name.clone()).or_insert(0) += 1;

        let severity_str = format!("{:?}", ap.severity);
        let file_path = ap
            .affected_nodes
            .first()
            .map_or("(unknown)", |s| s.as_str());
        let detail = if ap.evidence.is_empty() {
            ap.description.clone()
        } else {
            ap.evidence.join("; ")
        };

        critical_items.push(AntiPatternItem {
            pattern_type: ap.pattern_name.clone(),
            file_path: file_path.to_string(),
            node_name: ap.affected_nodes.first().cloned().unwrap_or_default(),
            severity: severity_str,
            detail,
            recommendation: ap.refactoring_steps.join(" → "),
        });
    }

    // Build migration impact statements
    for (name, count) in &by_type {
        let impact = match name.as_str() {
            "God Object" => format!(
                "{count} God Object pages should be split BEFORE migration (Wave 0 refactoring)"
            ),
            "Session Soup" => format!(
                "Session Soup keys must be consolidated before parallel wave execution ({count} instances)"
            ),
            "Spaghetti Events" => format!(
                "{count} Spaghetti Event chains indicate hidden coupling — verify with characterization tests"
            ),
            "SqlDataSource Coupling" => format!(
                "{count} SqlDataSource usages have inline SQL — extract to repository pattern"
            ),
            "Tight GIS Coupling" => {
                format!("{count} tightly coupled GIS components — extract map service layer")
            }
            "Windows Service" => {
                format!("{count} Windows Services — migrate to IHostedService / BackgroundService")
            }
            _ => format!("{count} {name} instances detected"),
        };
        migration_impact.push(impact);
    }

    AntiPatternSummary {
        total_anti_patterns: detected.len(),
        by_type,
        critical_items,
        migration_impact,
    }
}
