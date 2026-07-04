//! Analyzer submodules. Each builds a slice of the final
//! [`super::FullProjectMigrationReport`] (data access, JS, GIS,
//! web.config, etc). The orchestrator `analyze_full_project` in the
//! parent module calls these in sequence.
//!
//! Phase 2 of a structural split; see the parent file's header
//! comment for the full plan.

pub(super) mod anti_patterns;
pub(super) mod background_jobs;
pub(super) mod caching;
pub(super) mod classic_asp;
pub(super) mod common;
pub(super) mod config_transforms;
pub(super) mod cross_cutting;
pub(super) mod cross_layer;
pub(super) mod dependencies;
pub(super) mod email;
pub(super) mod endpoints;
pub(super) mod gis;
pub(super) mod global_asax;
pub(super) mod inheritance;
pub(super) mod js;
pub(super) mod master_pages;
pub(super) mod methods;
pub(super) mod multi_tenancy;
pub(super) mod reports;
pub(super) mod resources;
pub(super) mod routing;
pub(super) mod sp_catalog;
pub(super) mod third_party;
pub(super) mod vb_translation;
pub(super) mod web_config;
