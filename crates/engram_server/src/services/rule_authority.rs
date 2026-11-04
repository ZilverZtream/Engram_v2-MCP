//! Describe stored provenance without inferring policy acceptance from an ID,
//! filename, rule wording or source-file existence.
use serde::Serialize;

pub const MANUAL_PROVENANCE: &str = "manual repository rule";
pub(crate) const SOURCE_PROVENANCE: &str = "quality-gate source: ";

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Authority {
    AcceptedManual,
    SourceReference,
    LearnedEvidence,
    LegacyUnverified,
}

/// SourceReference describes a claim, not a successful receipt/freshness check.
pub(crate) fn classify(provenance: Option<&str>) -> Authority {
    match provenance {
        Some("manual" | "user-authored" | MANUAL_PROVENANCE) => Authority::AcceptedManual,
        Some(p) if p.starts_with(SOURCE_PROVENANCE) => Authority::SourceReference,
        Some(p)
            if p.starts_with("recurring review cluster ")
                || p == "revert-derived rule"
                || p == "manually ingested review finding" =>
        {
            Authority::LearnedEvidence
        }
        _ => Authority::LegacyUnverified,
    }
}
