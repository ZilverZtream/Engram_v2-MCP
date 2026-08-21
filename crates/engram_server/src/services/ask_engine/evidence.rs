//! Typed evidence — the unit ask_engine orchestrates over. Providers produce
//! `EvidenceItem`s directly from the substrate; nothing parses Markdown.

use serde::Serialize;

/// What KIND of thing this evidence is (drives rendering + directness scoring).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    SourceCode,
    DocSection,
    MemoryNote,
    Insight,
    BusinessRule,
    HistoryCommit,
    GraphRelation,
    ConceptGroup,
    TestRef,
    Setting,
}

/// Trust precedence. ORDER MATTERS: `Ord` follows declaration order, so the
/// most-authoritative variant is the smallest and sorts first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Authority {
    RuntimeEvidence,      // observed runtime at the requested version
    CurrentCode,          // current executable code + tests
    ApprovedRequirement,  // approved requirement / explicit decision (memory)
    CurrentDocs,          // current product documentation
    MergedHistory,        // merged implementation history
    DerivedBusinessLogic, // validated derived business logic
    AgentMemory,          // agent-authored memory note
    DreamerInsight,       // dreamer insight
    SemanticSimilarity,   // weak: similarity only
}

impl Authority {
    /// 1.0 (strongest) .. 0.15 (weakest) — a monotonic weight for scoring.
    pub fn weight(self) -> f32 {
        match self {
            Authority::RuntimeEvidence => 1.0,
            Authority::CurrentCode => 0.95,
            Authority::ApprovedRequirement => 0.9,
            Authority::CurrentDocs => 0.7,
            Authority::MergedHistory => 0.6,
            Authority::DerivedBusinessLogic => 0.55,
            Authority::AgentMemory => 0.45,
            Authority::DreamerInsight => 0.35,
            Authority::SemanticSimilarity => 0.15,
        }
    }
}

/// One piece of evidence with full provenance. `score`/`directness` are filled
/// by the ranker, not the provider.
#[derive(Debug, Clone, Serialize)]
pub struct EvidenceItem {
    pub evidence_id: String, // "ev_<n>"
    pub kind: EvidenceKind,
    pub authority: Authority,
    pub path: Option<String>,
    pub lines: Option<(u32, u32)>,
    pub symbol_id: Option<String>,
    pub title: Option<String>,
    pub content: String, // bounded snippet
    pub generation: Option<u64>,
    pub commit: Option<String>,
    pub timestamp: Option<u64>,
    pub confidence: f32,           // extraction/retrieval confidence 0..1
    pub relevance: f32,            // query relevance from the arm 0..1
    pub extraction_method: String, // ast | fts | vector | graph | git | memory
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    pub provider: String, // arm that produced it
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directness: Option<f32>,
}
