#![deny(clippy::print_stdout)]

pub mod dreaming;
pub mod embed;
pub mod immune;
pub mod llm_provider;
pub mod mimicry;

pub use dreaming::{DreamInsight, DreamingEngine, MigrationBoundary};
pub use embed::{
    Embedder, Embedding, LocalEmbedder, OllamaEmbedder, OpenAIEmbedder, ProjectionEmbedder,
    RemoteEmbedder, build_embedder,
};
pub use immune::{ImmuneDecision, ImmuneEngine};
pub use mimicry::{StyleGuide, StyleMimicryEngine};
