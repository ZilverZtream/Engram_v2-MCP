#![deny(clippy::print_stdout)]

pub mod dreaming;
pub mod embed;
pub mod immune;
pub mod mimicry;

pub use dreaming::{DreamInsight, DreamingEngine};
pub use embed::{Embedder, Embedding};
pub use immune::{ImmuneDecision, ImmuneEngine};
pub use mimicry::{StyleGuide, StyleMimicryEngine};
