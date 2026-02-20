#![deny(clippy::print_stdout)]
#![deny(clippy::unwrap_used)]

pub mod chunking;
pub mod ddl_extractor;
pub mod docstore;
pub mod hybrid;
pub mod ingest;
pub mod layout_extractor;
pub mod parsing;
pub mod state_extractor;
pub mod tantivy_index;
pub mod vb_extractor;
#[cfg(feature = "vector")]
pub mod vector;
pub mod webforms;

pub use docstore::{DocRecord, DocStore, FileFingerprint};
pub use hybrid::{
    HybridHit, HybridQuery, HybridSearchEngine, IndexDoc, IngestStats, chunk_id_from_content_hash,
    chunk_id_from_hash, escape_tantivy_literal,
};
pub use parsing::{ExtractedEdge, ExtractedSymbol, SymbolExtractor};
