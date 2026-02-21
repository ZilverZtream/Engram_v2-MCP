//! Extraction confidence scoring for WebForms and legacy ASP.NET patterns.
//!
//! Assigns a confidence score (0.0 – 1.0) to each extracted symbol/edge based
//! on how reliably the extractor could resolve the wiring. High-confidence
//! extractions have explicit markup/code evidence; low-confidence ones rely on
//! heuristic name matching or incomplete parse trees.

use serde::{Deserialize, Serialize};

/// Confidence band for extraction results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfidenceBand {
    /// >= 0.8: Strong evidence (explicit `Inherits`, direct control ID, etc.)
    High,
    /// 0.5 – 0.8: Reasonable evidence (naming convention, partial parse)
    Medium,
    /// < 0.5: Weak evidence (heuristic matching, fallback resolution)
    Low,
}

impl ConfidenceBand {
    pub fn from_score(score: f64) -> Self {
        if score >= 0.8 {
            Self::High
        } else if score >= 0.5 {
            Self::Medium
        } else {
            Self::Low
        }
    }
}

impl std::fmt::Display for ConfidenceBand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::High => write!(f, "high"),
            Self::Medium => write!(f, "medium"),
            Self::Low => write!(f, "low"),
        }
    }
}

/// Detailed confidence score for an extraction result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionConfidence {
    /// Overall confidence (0.0 – 1.0).
    pub score: f64,
    /// Confidence band classification.
    pub band: ConfidenceBand,
    /// Individual signal scores contributing to the overall confidence.
    pub signals: Vec<ConfidenceSignal>,
    /// Human-readable rationale for the score.
    pub rationale: String,
}

/// A single signal contributing to extraction confidence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfidenceSignal {
    pub name: String,
    pub score: f64,
    pub weight: f64,
    pub evidence: String,
}

/// Score extraction confidence for WebForms event wiring.
pub fn score_event_wiring(
    has_inherits_directive: bool,
    has_codebehind_file: bool,
    has_matching_handler: bool,
    handler_signature_valid: bool,
    control_id_explicit: bool,
) -> ExtractionConfidence {
    let mut signals = Vec::new();
    let mut total_weight = 0.0;
    let mut weighted_sum = 0.0;

    // Signal 1: Inherits directive present
    let inherits_score = if has_inherits_directive { 1.0 } else { 0.0 };
    signals.push(ConfidenceSignal {
        name: "inherits_directive".into(),
        score: inherits_score,
        weight: 0.25,
        evidence: if has_inherits_directive {
            "Page has explicit Inherits directive".into()
        } else {
            "No Inherits directive found — codebehind class is ambiguous".into()
        },
    });
    weighted_sum += inherits_score * 0.25;
    total_weight += 0.25;

    // Signal 2: Codebehind file exists
    let cb_score = if has_codebehind_file { 1.0 } else { 0.0 };
    signals.push(ConfidenceSignal {
        name: "codebehind_file".into(),
        score: cb_score,
        weight: 0.2,
        evidence: if has_codebehind_file {
            "Codebehind .cs/.vb file found on disk".into()
        } else {
            "Codebehind file not found — may be compiled into DLL".into()
        },
    });
    weighted_sum += cb_score * 0.2;
    total_weight += 0.2;

    // Signal 3: Handler method exists with matching name
    let handler_score = if has_matching_handler { 1.0 } else { 0.0 };
    signals.push(ConfidenceSignal {
        name: "handler_match".into(),
        score: handler_score,
        weight: 0.25,
        evidence: if has_matching_handler {
            "Event handler method found in codebehind".into()
        } else {
            "Handler method not found — may be inherited or dynamically wired".into()
        },
    });
    weighted_sum += handler_score * 0.25;
    total_weight += 0.25;

    // Signal 4: Handler signature is valid
    let sig_score = if handler_signature_valid { 1.0 } else { 0.3 };
    signals.push(ConfidenceSignal {
        name: "handler_signature".into(),
        score: sig_score,
        weight: 0.15,
        evidence: if handler_signature_valid {
            "Handler has standard EventHandler signature".into()
        } else {
            "Handler signature does not match expected pattern".into()
        },
    });
    weighted_sum += sig_score * 0.15;
    total_weight += 0.15;

    // Signal 5: Control ID is explicit (not auto-generated)
    let ctrl_score = if control_id_explicit { 1.0 } else { 0.4 };
    signals.push(ConfidenceSignal {
        name: "control_id_explicit".into(),
        score: ctrl_score,
        weight: 0.15,
        evidence: if control_id_explicit {
            "Control has explicit runat=server ID attribute".into()
        } else {
            "Control ID may be auto-generated or inherited".into()
        },
    });
    weighted_sum += ctrl_score * 0.15;
    total_weight += 0.15;

    let overall = if total_weight > 0.0 {
        weighted_sum / total_weight
    } else {
        0.0
    };

    let band = ConfidenceBand::from_score(overall);

    let rationale = match band {
        ConfidenceBand::High => {
            "Strong evidence for correct event wiring extraction".into()
        }
        ConfidenceBand::Medium => {
            "Reasonable confidence but some signals missing — manual verification recommended".into()
        }
        ConfidenceBand::Low => {
            "Low confidence — significant uncertainty in event wiring. Agent should verify manually.".into()
        }
    };

    ExtractionConfidence {
        score: overall,
        band,
        signals,
        rationale,
    }
}

/// Score extraction confidence for SQL path traces.
pub fn score_sql_trace(
    has_explicit_connection_string: bool,
    has_parameterized_query: bool,
    table_name_resolved: bool,
    column_names_resolved: bool,
    stored_proc_verified: bool,
) -> ExtractionConfidence {
    let mut signals = Vec::new();
    let mut weighted_sum = 0.0;
    let mut total_weight = 0.0;

    let conn_score = if has_explicit_connection_string {
        1.0
    } else {
        0.3
    };
    signals.push(ConfidenceSignal {
        name: "connection_string".into(),
        score: conn_score,
        weight: 0.2,
        evidence: if has_explicit_connection_string {
            "Explicit connection string found in config".into()
        } else {
            "Connection string not found — may use implicit or web.config inheritance".into()
        },
    });
    weighted_sum += conn_score * 0.2;
    total_weight += 0.2;

    let param_score = if has_parameterized_query { 1.0 } else { 0.5 };
    signals.push(ConfidenceSignal {
        name: "parameterized_query".into(),
        score: param_score,
        weight: 0.15,
        evidence: if has_parameterized_query {
            "Query uses parameterized SQL (SqlParameter)".into()
        } else {
            "Query may use string concatenation — injection risk".into()
        },
    });
    weighted_sum += param_score * 0.15;
    total_weight += 0.15;

    let table_score = if table_name_resolved { 1.0 } else { 0.2 };
    signals.push(ConfidenceSignal {
        name: "table_resolution".into(),
        score: table_score,
        weight: 0.25,
        evidence: if table_name_resolved {
            "Table name resolved to schema definition".into()
        } else {
            "Table name could not be resolved — may be dynamic or use synonym".into()
        },
    });
    weighted_sum += table_score * 0.25;
    total_weight += 0.25;

    let col_score = if column_names_resolved { 1.0 } else { 0.3 };
    signals.push(ConfidenceSignal {
        name: "column_resolution".into(),
        score: col_score,
        weight: 0.2,
        evidence: if column_names_resolved {
            "Column names resolved to table schema".into()
        } else {
            "Column names unresolved — SELECT * or dynamic column access".into()
        },
    });
    weighted_sum += col_score * 0.2;
    total_weight += 0.2;

    let proc_score = if stored_proc_verified { 1.0 } else { 0.5 };
    signals.push(ConfidenceSignal {
        name: "stored_proc_verification".into(),
        score: proc_score,
        weight: 0.2,
        evidence: if stored_proc_verified {
            "Stored procedure verified in DDL or database schema".into()
        } else {
            "Stored procedure existence not verified".into()
        },
    });
    weighted_sum += proc_score * 0.2;
    total_weight += 0.2;

    let overall = if total_weight > 0.0 {
        weighted_sum / total_weight
    } else {
        0.0
    };
    let band = ConfidenceBand::from_score(overall);

    let rationale = match band {
        ConfidenceBand::High => "SQL path fully traced with high confidence".into(),
        ConfidenceBand::Medium => "SQL path partially traced — some references unresolved".into(),
        ConfidenceBand::Low => {
            "SQL path weakly traced — significant ambiguity in data access layer".into()
        }
    };

    ExtractionConfidence {
        score: overall,
        band,
        signals,
        rationale,
    }
}

/// Score confidence for control ID binding (WebForms).
pub fn score_control_binding(
    runat_server_present: bool,
    id_attribute_explicit: bool,
    designer_file_has_field: bool,
    codebehind_references_control: bool,
) -> ExtractionConfidence {
    let mut signals = Vec::new();
    let mut weighted_sum = 0.0;
    let mut total_weight = 0.0;

    let runat_score = if runat_server_present { 1.0 } else { 0.0 };
    signals.push(ConfidenceSignal {
        name: "runat_server".into(),
        score: runat_score,
        weight: 0.3,
        evidence: if runat_server_present {
            "Control has runat=\"server\" attribute".into()
        } else {
            "No runat=\"server\" — control is client-side only".into()
        },
    });
    weighted_sum += runat_score * 0.3;
    total_weight += 0.3;

    let id_score = if id_attribute_explicit { 1.0 } else { 0.2 };
    signals.push(ConfidenceSignal {
        name: "explicit_id".into(),
        score: id_score,
        weight: 0.25,
        evidence: if id_attribute_explicit {
            "Control has explicit ID attribute".into()
        } else {
            "Control ID is auto-generated or missing".into()
        },
    });
    weighted_sum += id_score * 0.25;
    total_weight += 0.25;

    let designer_score = if designer_file_has_field { 1.0 } else { 0.4 };
    signals.push(ConfidenceSignal {
        name: "designer_field".into(),
        score: designer_score,
        weight: 0.25,
        evidence: if designer_file_has_field {
            "Control declared in .designer.cs/.designer.vb".into()
        } else {
            "No designer field found — may be dynamically created".into()
        },
    });
    weighted_sum += designer_score * 0.25;
    total_weight += 0.25;

    let ref_score = if codebehind_references_control {
        1.0
    } else {
        0.3
    };
    signals.push(ConfidenceSignal {
        name: "codebehind_reference".into(),
        score: ref_score,
        weight: 0.2,
        evidence: if codebehind_references_control {
            "Codebehind references this control by ID".into()
        } else {
            "No codebehind reference found for this control".into()
        },
    });
    weighted_sum += ref_score * 0.2;
    total_weight += 0.2;

    let overall = if total_weight > 0.0 {
        weighted_sum / total_weight
    } else {
        0.0
    };
    let band = ConfidenceBand::from_score(overall);

    let rationale = match band {
        ConfidenceBand::High => "Control binding fully verified".into(),
        ConfidenceBand::Medium => "Control binding partially verified — check designer file".into(),
        ConfidenceBand::Low => "Control binding uncertain — may be dynamic or inherited".into(),
    };

    ExtractionConfidence {
        score: overall,
        band,
        signals,
        rationale,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perfect_event_wiring_is_high() {
        let c = score_event_wiring(true, true, true, true, true);
        assert!(c.score >= 0.8);
        assert_eq!(c.band, ConfidenceBand::High);
    }

    #[test]
    fn missing_handler_drops_to_medium() {
        let c = score_event_wiring(true, true, false, true, true);
        assert!(c.score < 0.8);
        assert!(c.score >= 0.5);
        assert_eq!(c.band, ConfidenceBand::Medium);
    }

    #[test]
    fn nothing_found_is_low() {
        let c = score_event_wiring(false, false, false, false, false);
        assert!(c.score < 0.5);
        assert_eq!(c.band, ConfidenceBand::Low);
    }

    #[test]
    fn sql_trace_all_resolved_is_high() {
        let c = score_sql_trace(true, true, true, true, true);
        assert!(c.score >= 0.8);
        assert_eq!(c.band, ConfidenceBand::High);
    }

    #[test]
    fn control_binding_no_runat_is_low() {
        let c = score_control_binding(false, false, false, false);
        assert!(c.score < 0.5);
        assert_eq!(c.band, ConfidenceBand::Low);
    }
}
