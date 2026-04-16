#![deny(clippy::print_stdout)]
#![deny(clippy::unwrap_used)]
#![cfg_attr(test, allow(clippy::unwrap_used))]

pub mod asp_classic_extractor;
pub mod chunking;
pub mod confidence;
pub mod config_extractor;
pub mod control_mapping;
pub mod cs_extractor;
pub mod ddl_extractor;
pub mod docstore;
pub mod hybrid;
pub mod ingest;
pub mod jquery_inventory;
pub mod js_extractor;
pub mod language_diagnostics;
pub mod layout_extractor;
pub mod parsing;
pub mod report_extractor;
pub mod solution_parser;
pub mod sp_extractor;
pub mod sql_parser;
pub mod state_extractor;
pub mod sync_hazard_detector;
pub mod tantivy_index;
pub mod vb_extractor;
pub mod vb_translation_traps;
#[cfg(feature = "vector")]
pub mod vector;
#[cfg(feature = "vector")]
pub use vector::TableOpenOutcome;
pub mod webforms;

pub use confidence::{
    ConfidenceBand, ConfidenceSignal, ExtractionConfidence, score_control_binding,
    score_event_wiring, score_sql_trace,
};
pub use docstore::{DocRecord, DocStore, FileFingerprint};
pub use hybrid::{
    BulkWriterGuard, HybridHit, HybridQuery, HybridSearchEngine, IndexDoc, IngestStats,
    chunk_id_from_content_hash, chunk_id_from_hash, escape_tantivy_literal,
};
pub use parsing::{ExtractedEdge, ExtractedSymbol, SymbolExtractor};
