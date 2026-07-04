//! Extracted analyzer: reports.
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

pub(crate) fn build_report_summary(
    graph: &Arc<GraphStore>,
    project_id: &str,
    report_files: &[(String, String)],
) -> ReportSummary {
    // Query graph for report-related insights
    let all_insights = nodes_or_warn(
        graph.query_nodes(project_id, Some("insight"), None, None, 2_000),
        "report_insights",
    );

    let report_insights: Vec<_> = all_insights
        .iter()
        .filter(|n| {
            let name = n.name.to_lowercase();
            name.contains("report")
                || name.contains("crystal")
                || name.contains("ssrs")
                || name.contains("rdl")
        })
        .collect();

    // Also query for anti-pattern edges related to Crystal Reports
    let ap_edges = edges_or_warn(
        graph.list_edges_by_kind(project_id, EdgeKind::AntiPattern, 5_000),
        "AntiPattern/reports",
    );
    let crystal_edges: Vec<_> = ap_edges
        .iter()
        .filter(|e| {
            e.metadata
                .as_ref()
                .and_then(|m| m.get("pattern").and_then(|v| v.as_str()))
                .is_some_and(|p| p.to_lowercase().contains("crystal"))
        })
        .collect();

    let mut ssrs_reports = Vec::new();
    let mut crystal_reports = Vec::new();
    let mut shared_data_sources = Vec::new();
    let mut has_binary_rpt = false;

    // Parse SSRS report files (.rdl, .rdlc)
    for (path, content) in report_files {
        let ext = path.rsplit('.').next().unwrap_or("").to_lowercase();
        if ext == "rdl" || ext == "rdlc" {
            let datasets: Vec<String> = RPT_DATASET_RE
                .captures_iter(content)
                .map(|c| c[1].to_string())
                .collect();
            let param_count = RPT_PARAM_RE.find_iter(content).count();
            let subreports: Vec<String> = RPT_SUBREPORT_RE
                .captures_iter(content)
                .map(|c| c[1].to_string())
                .collect();
            for cap in RPT_DATASOURCE_RE.captures_iter(content) {
                let ds = cap[1].to_string();
                if !shared_data_sources.contains(&ds) {
                    shared_data_sources.push(ds);
                }
            }
            ssrs_reports.push(ReportInfo {
                file_path: path.clone(),
                datasets,
                parameters: param_count,
                subreports,
                migration_target: if ext == "rdlc" {
                    "SSRS on modern / Power BI paginated".into()
                } else {
                    "Power BI / SSRS on modern SQL Server".into()
                },
            });
        }
    }

    // Crystal Reports from graph insights and anti-pattern edges
    for insight in &report_insights {
        let name = &insight.name;
        if name.to_lowercase().contains("crystal") {
            let file = insight.file_path.as_str().to_string();
            let rpt_file = insight
                .metadata
                .as_ref()
                .and_then(|m| m.get("report_file").and_then(|v| v.as_str()))
                .unwrap_or("")
                .to_string();
            if rpt_file.ends_with(".rpt") {
                has_binary_rpt = true;
            }
            crystal_reports.push(CrystalReportInfo {
                file_path: file,
                report_file: rpt_file,
                is_binary: true,
                modern_equivalent: "Power BI / SSRS / Telerik Reporting".into(),
            });
        }
    }

    // Also detect Crystal from anti-pattern edges
    for edge in &crystal_edges {
        let file = super::common::extract_file_from_node_id(&edge.source_id);
        if !crystal_reports.iter().any(|cr| cr.file_path == file) {
            crystal_reports.push(CrystalReportInfo {
                file_path: file,
                report_file: String::new(),
                is_binary: true,
                modern_equivalent: "Power BI / SSRS / Telerik Reporting".into(),
            });
            has_binary_rpt = true;
        }
    }

    let total = ssrs_reports.len() + crystal_reports.len();

    ReportSummary {
        ssrs_reports,
        crystal_reports,
        total_reports: total,
        has_binary_rpt_files: has_binary_rpt,
        shared_data_sources,
    }
}
