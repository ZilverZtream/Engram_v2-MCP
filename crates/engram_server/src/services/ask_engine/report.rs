//! The assembled result. In M1 `mode` is always "retrieval_only": a typed,
//! ranked, honestly-statused evidence report — never concatenated prose passed
//! off as a synthesized answer. Render fns are added in Task 10.

use serde::Serialize;

use super::evidence::EvidenceItem;
use super::plan::QueryPlan;
use super::status::{AnswerStatus, Conflict, FreshnessSnapshot, ProviderReport};

#[derive(Debug, Clone, Serialize)]
pub struct AskReport {
    pub question: String,
    pub plan: QueryPlan,
    pub status: AnswerStatus,
    pub mode: String, // "retrieval_only" (M1) | "synthesized" (M2)
    pub evidence: Vec<EvidenceItem>, // ranked, deduped, bounded (high-signal)
    pub conflicts: Vec<Conflict>,
    pub unknowns: Vec<String>, // coverage gaps
    pub next_best: Vec<String>, // suggested follow-up investigations
    pub snapshot: FreshnessSnapshot,
    pub providers: Vec<ProviderReport>,
}
