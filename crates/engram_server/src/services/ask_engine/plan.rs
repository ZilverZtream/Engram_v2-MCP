//! Query understanding — the deterministic, multi-intent, multi-entity plan the
//! planner produces and the retrieval DAGs consume.

use serde::Serialize;

use super::evidence::EvidenceKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    Explain,
    Impact,
    Usage,
    History,
    Rationale, // "why is it THIS way" — decisions/rationale, ≠ History
    Feature,
    BugDiagnosis,
    Requirements,
    Compare,
    Test,
    Unknowns,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    Symbol,
    File,
    Route,
    Table,
    Column,
    Setting,
    UiControl,
    Concept,
    Requirement,
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
pub struct EntityMention {
    pub text: String,             // as found in the question
    pub guessed_kind: EntityKind, // from surface form
    pub resolved: Vec<ResolvedEntity>, // 0 = unresolved, >1 = ambiguous branch
}

#[derive(Debug, Clone, Serialize)]
pub struct ResolvedEntity {
    pub kind: EntityKind,
    pub canonical: String, // fqn / path / table name
    pub node_id: Option<String>,
    pub confidence: f32,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Qualifiers {
    pub roles: Vec<String>,                // admin, tenant-admin, user
    pub change: Option<(String, String)>,  // (from, to): XML → JSON
    pub scopes: Vec<String>,               // import, export, ...
    pub symptoms: Vec<String>,             // error strings / symptoms
    pub versions: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerType {
    Explanation,
    ImpactSet,
    UsageSites,
    Timeline,
    Rationale,
    Plan,
    RootCause,
    RequirementRef,
    Comparison,
    TestGuidance,
    CoverageGaps,
}

#[derive(Debug, Clone, Serialize)]
pub struct QueryPlan {
    pub intents: Vec<(Intent, f32)>, // weighted SET
    pub entities: Vec<EntityMention>,
    pub qualifiers: Qualifiers,
    pub needed_evidence: Vec<EvidenceKind>,
    pub answer_type: AnswerType,
}
