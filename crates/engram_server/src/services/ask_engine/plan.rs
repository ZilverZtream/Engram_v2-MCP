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
    pub text: String,                  // as found in the question
    pub guessed_kind: EntityKind,      // from surface form
    pub resolved: Vec<ResolvedEntity>, // 0 = unresolved, >1 = ambiguous branch
}

#[derive(Debug, Clone, Serialize)]
pub struct ResolvedEntity {
    pub kind: EntityKind,
    pub canonical: String, // fqn / path / table name
    pub node_id: Option<String>,
    pub confidence: f32,
}

/// Doc-13 Phase A (round-3 audit item 1): the typed ANSWER CONTRACT — what
/// SHAPE of answer the question demands. Traversal policy (Phase C) and
/// status validation (Phase D) key off this, never off intent heuristics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ContractDirection {
    None,
    Callers,
    Callees,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ContractEntityType {
    Any,
    Function,
    File,
    Table,
    Route,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Cardinality {
    One,
    TopK,
    ExhaustiveSet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Facet {
    Definition,
    Caller,
    Implementation,
    Rationale,
}

#[derive(Debug, Clone, Serialize)]
pub struct AnswerContract {
    pub direction: ContractDirection,
    pub entity_type: ContractEntityType,
    pub cardinality: Cardinality,
    pub required_facets: Vec<Facet>,
    /// Empty = every evidence class is admissible.
    pub allowed_evidence: Vec<EvidenceKind>,
    pub completeness_required: bool,
}

impl Default for AnswerContract {
    fn default() -> Self {
        Self {
            direction: ContractDirection::None,
            entity_type: ContractEntityType::Any,
            cardinality: Cardinality::TopK,
            required_facets: Vec::new(),
            allowed_evidence: Vec::new(),
            completeness_required: false,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Qualifiers {
    /// Batch 7 (doc 11, live r63 usage_5): "under ts/map" — directory-shaped
    /// scopes the question names; retrieval honors them pool-wide.
    pub path_prefixes: Vec<String>,
    pub roles: Vec<String>,               // admin, tenant-admin, user
    pub change: Option<(String, String)>, // (from, to): XML → JSON
    pub scopes: Vec<String>,              // import, export, ...
    pub symptoms: Vec<String>,            // error strings / symptoms
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
    /// Round-2 audit P0-4: the evidence modalities the question asks for.
    pub modalities: Vec<Modality>,
    /// Doc-13 Phase A: the typed answer contract derived from the question's shape.
    pub contract: AnswerContract,
}

/// Round-2 audit P0-4: the evidence MODALITY a question names — "which
/// reports (.rdl) …", "which table …", "which resource keys …". Retrieval
/// runs a modality-filtered arm for each, the ranker keeps its best item,
/// and an answer with no evidence of a requested modality is at most Partial.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Modality {
    Sql,
    Report,
    Resource,
    Markup,
    Script,
}

impl Modality {
    pub fn suffixes(self) -> &'static [&'static str] {
        match self {
            Modality::Sql => &[".sql", ".dbml", ".edmx"],
            Modality::Report => &[".rdl", ".rdlc"],
            Modality::Resource => &[".resx"],
            Modality::Markup => &[".aspx", ".ascx", ".master", ".cshtml", ".vbhtml", ".html"],
            Modality::Script => &[".ts", ".tsx", ".js", ".jsx"],
        }
    }

    /// Stable provider id ("modality:<id>").
    pub fn id(self) -> &'static str {
        match self {
            Modality::Sql => "sql",
            Modality::Report => "report",
            Modality::Resource => "resource",
            Modality::Markup => "markup",
            Modality::Script => "script",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Modality::Sql => "SQL schema (.sql/.dbml)",
            Modality::Report => "report (.rdl)",
            Modality::Resource => "resource (.resx)",
            Modality::Markup => "page markup (.aspx/.ascx)",
            Modality::Script => "script (.ts/.js)",
        }
    }

    pub fn matches(self, path: &str) -> bool {
        let p = path.to_lowercase();
        self.suffixes().iter().any(|s| p.ends_with(s))
    }
}
