//! ask_engine — the deterministic evidence engine behind `ask_codebase`.
//!
//! Pipeline: plan (multi-intent) → resolve entities → parallel typed retrieval
//! → dedup + authority/directness ranking + conflict detection → honest status
//! + freshness snapshot → `retrieval_only` report. Producers emit typed
//! `EvidenceItem`s straight from the substrate; nothing here parses Markdown.

pub mod evidence;
pub mod plan;
pub mod planner;
pub mod providers;
pub mod ranking;
pub mod report;
pub mod resolver;
pub mod retrieval;
pub mod status;
