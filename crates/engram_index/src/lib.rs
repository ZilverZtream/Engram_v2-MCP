#![deny(clippy::print_stdout)]
#![deny(clippy::unwrap_used)]
#![cfg_attr(test, allow(clippy::unwrap_used))]

pub mod asp_classic_extractor;
pub mod asset_bundles;
/// Source chunk ranges, qualified call binding and source-edge replacement
/// contract, including parsed member ownership and declaration-specific FQNs. Older
/// metadata must be re-extracted even with unchanged source. Version 7 adds
/// cross-artifact ASP.NET Optimization bundle/render edges.
pub const SOURCE_INDEX_VERSION: u64 = 7;
pub mod chunking;
pub mod confidence;
pub mod config_extractor;
pub mod control_mapping;
pub mod cs_extractor;
pub mod ddl_extractor;
pub mod docstore;
pub mod embed_cache;
pub mod grep;
pub mod hybrid;
pub mod ingest;
pub mod jquery_inventory;
pub mod js_extractor;
pub mod language_diagnostics;
pub mod layout_extractor;
pub mod ml_extractor;
pub mod parsing;
pub mod quality_gates;
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
pub mod word_tokenizer;

pub use confidence::{
    ConfidenceBand, ConfidenceSignal, ExtractionConfidence, score_control_binding,
    score_event_wiring, score_sql_trace,
};
pub use docstore::{DocRecord, DocStore, FileFingerprint};
pub use hybrid::{
    BulkWriterGuard, HybridHit, HybridQuery, HybridSearchEngine, IndexDoc, IngestStats, StoredDoc,
    SemanticQuality, chunk_id_from_content_hash, chunk_id_from_hash, escape_tantivy_literal, literal_text_query,
    semantic_quality_for_backend,
};
pub use parsing::{ExtractedEdge, ExtractedSymbol, SymbolExtractor};
