//! Runtime evidence schema — normalized format for runtime path confirmation.
//!
//! Defines the canonical data structures for runtime events that can be
//! reconciled against static trace results to uplift ADP confidence.

use serde::{Deserialize, Serialize};

/// A single runtime event observed during application execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeEvent {
    /// Unique event identifier.
    pub event_id: String,
    /// ISO-8601 or epoch-ms timestamp.
    pub timestamp: String,
    /// Event category: "control_interaction", "route", "sql_execution", "state_mutation".
    pub event_type: RuntimeEventType,
    /// Source file path (project-relative) where the event originated.
    pub source_path: String,
    /// Function or method name at the event site.
    pub source_function: Option<String>,
    /// Line number in source file (if available).
    pub source_line: Option<u32>,
    /// Target of the event (e.g., SQL table, state key, route path).
    pub target: Option<String>,
    /// Additional context (key=value pairs).
    pub context: std::collections::HashMap<String, String>,
    /// Trust level of this evidence source (0.0–1.0).
    pub trust_weight: f64,
}

/// Runtime event type categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuntimeEventType {
    /// User interaction with a UI control (click, change, etc.).
    ControlInteraction,
    /// HTTP route was hit.
    Route,
    /// SQL query or stored procedure was executed.
    SqlExecution,
    /// Application state was read or written (Session, ViewState, etc.).
    StateMutation,
}

impl RuntimeEventType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ControlInteraction => "control_interaction",
            Self::Route => "route",
            Self::SqlExecution => "sql_execution",
            Self::StateMutation => "state_mutation",
        }
    }
}

/// A batch of runtime events for ingestion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeEvidenceBatch {
    /// Schema version for forward compatibility.
    pub schema_version: String,
    /// Project this evidence belongs to.
    pub project_id: String,
    /// Collection session identifier.
    pub session_id: String,
    /// Events in chronological order.
    pub events: Vec<RuntimeEvent>,
}

/// Result of reconciling runtime events against static trace predictions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconciliationResult {
    /// Total static trace paths evaluated.
    pub static_paths_count: usize,
    /// Paths confirmed by runtime evidence.
    pub confirmed_count: usize,
    /// Paths contradicted by runtime evidence.
    pub contradicted_count: usize,
    /// Paths with no runtime evidence (inconclusive).
    pub inconclusive_count: usize,
    /// Confidence uplift factor (positive = more confident).
    pub confidence_delta: f64,
    /// Per-path reconciliation details.
    pub details: Vec<PathReconciliation>,
}

/// Per-path reconciliation detail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathReconciliation {
    /// Static trace path identifier.
    pub path_id: String,
    /// Reconciliation status.
    pub status: ReconciliationStatus,
    /// Matching runtime event IDs (if confirmed).
    pub matching_event_ids: Vec<String>,
    /// Reason for the status.
    pub reason: String,
}

/// Reconciliation status for a single trace path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReconciliationStatus {
    /// Runtime evidence confirms the static prediction.
    Confirmed,
    /// Runtime evidence contradicts the static prediction.
    Contradicted,
    /// No runtime evidence available for this path.
    Inconclusive,
}

/// Validate a runtime evidence batch for schema compliance.
pub fn validate_batch(batch: &RuntimeEvidenceBatch) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    if batch.schema_version.is_empty() {
        errors.push(ValidationError {
            field: "schema_version".into(),
            message: "schema_version must not be empty".into(),
        });
    }

    if batch.project_id.is_empty() {
        errors.push(ValidationError {
            field: "project_id".into(),
            message: "project_id must not be empty".into(),
        });
    }

    if batch.session_id.is_empty() {
        errors.push(ValidationError {
            field: "session_id".into(),
            message: "session_id must not be empty".into(),
        });
    }

    for (i, event) in batch.events.iter().enumerate() {
        if event.event_id.is_empty() {
            errors.push(ValidationError {
                field: format!("events[{i}].event_id"),
                message: "event_id must not be empty".into(),
            });
        }

        if event.timestamp.is_empty() {
            errors.push(ValidationError {
                field: format!("events[{i}].timestamp"),
                message: "timestamp must not be empty".into(),
            });
        }

        if event.source_path.is_empty() {
            errors.push(ValidationError {
                field: format!("events[{i}].source_path"),
                message: "source_path must not be empty".into(),
            });
        }

        if !(0.0..=1.0).contains(&event.trust_weight) {
            errors.push(ValidationError {
                field: format!("events[{i}].trust_weight"),
                message: format!("trust_weight must be 0.0–1.0, got {}", event.trust_weight),
            });
        }
    }

    errors
}

/// Schema validation error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationError {
    pub field: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_event() -> RuntimeEvent {
        RuntimeEvent {
            event_id: "evt001".into(),
            timestamp: "1708000000000".into(),
            event_type: RuntimeEventType::ControlInteraction,
            source_path: "Order.aspx.cs".into(),
            source_function: Some("btnSave_Click".into()),
            source_line: Some(42),
            target: Some("Orders".into()),
            context: [("control_id".into(), "btnSave".into())]
                .into_iter()
                .collect(),
            trust_weight: 0.9,
        }
    }

    fn valid_batch() -> RuntimeEvidenceBatch {
        RuntimeEvidenceBatch {
            schema_version: "1.0.0".into(),
            project_id: "test-project".into(),
            session_id: "session-001".into(),
            events: vec![valid_event()],
        }
    }

    #[test]
    fn valid_batch_passes_validation() {
        let errors = validate_batch(&valid_batch());
        assert!(
            errors.is_empty(),
            "Valid batch should have no errors: {:?}",
            errors
        );
    }

    #[test]
    fn empty_project_id_fails_validation() {
        let mut batch = valid_batch();
        batch.project_id = String::new();
        let errors = validate_batch(&batch);
        assert!(errors.iter().any(|e| e.field == "project_id"));
    }

    #[test]
    fn empty_event_id_fails_validation() {
        let mut batch = valid_batch();
        batch.events[0].event_id = String::new();
        let errors = validate_batch(&batch);
        assert!(errors.iter().any(|e| e.field.contains("event_id")));
    }

    #[test]
    fn invalid_trust_weight_fails_validation() {
        let mut batch = valid_batch();
        batch.events[0].trust_weight = 1.5;
        let errors = validate_batch(&batch);
        assert!(errors.iter().any(|e| e.field.contains("trust_weight")));
    }

    #[test]
    fn empty_source_path_fails_validation() {
        let mut batch = valid_batch();
        batch.events[0].source_path = String::new();
        let errors = validate_batch(&batch);
        assert!(errors.iter().any(|e| e.field.contains("source_path")));
    }

    #[test]
    fn batch_roundtrip_json() {
        let batch = valid_batch();
        let json = serde_json::to_string_pretty(&batch).unwrap();
        let decoded: RuntimeEvidenceBatch = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.events.len(), 1);
        assert_eq!(decoded.events[0].event_id, "evt001");
        assert_eq!(
            decoded.events[0].event_type,
            RuntimeEventType::ControlInteraction
        );
    }

    #[test]
    fn multiple_event_types_roundtrip() {
        let batch = RuntimeEvidenceBatch {
            schema_version: "1.0.0".into(),
            project_id: "test".into(),
            session_id: "s1".into(),
            events: vec![
                RuntimeEvent {
                    event_id: "e1".into(),
                    timestamp: "1".into(),
                    event_type: RuntimeEventType::ControlInteraction,
                    source_path: "a.cs".into(),
                    source_function: None,
                    source_line: None,
                    target: None,
                    context: Default::default(),
                    trust_weight: 1.0,
                },
                RuntimeEvent {
                    event_id: "e2".into(),
                    timestamp: "2".into(),
                    event_type: RuntimeEventType::SqlExecution,
                    source_path: "b.cs".into(),
                    source_function: Some("GetOrders".into()),
                    source_line: Some(10),
                    target: Some("dbo.Orders".into()),
                    context: [("command_type".into(), "StoredProcedure".into())]
                        .into_iter()
                        .collect(),
                    trust_weight: 0.8,
                },
                RuntimeEvent {
                    event_id: "e3".into(),
                    timestamp: "3".into(),
                    event_type: RuntimeEventType::StateMutation,
                    source_path: "c.cs".into(),
                    source_function: None,
                    source_line: None,
                    target: Some("Session[\"UserProfile\"]".into()),
                    context: [("operation".into(), "write".into())].into_iter().collect(),
                    trust_weight: 0.7,
                },
                RuntimeEvent {
                    event_id: "e4".into(),
                    timestamp: "4".into(),
                    event_type: RuntimeEventType::Route,
                    source_path: "d.cs".into(),
                    source_function: None,
                    source_line: None,
                    target: Some("/api/orders".into()),
                    context: [("method".into(), "POST".into())].into_iter().collect(),
                    trust_weight: 0.6,
                },
            ],
        };

        let json = serde_json::to_string(&batch).unwrap();
        let decoded: RuntimeEvidenceBatch = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.events.len(), 4);

        let errors = validate_batch(&decoded);
        assert!(errors.is_empty());
    }

    #[test]
    fn reconciliation_result_roundtrip() {
        let result = ReconciliationResult {
            static_paths_count: 5,
            confirmed_count: 3,
            contradicted_count: 1,
            inconclusive_count: 1,
            confidence_delta: 0.15,
            details: vec![
                PathReconciliation {
                    path_id: "path_001".into(),
                    status: ReconciliationStatus::Confirmed,
                    matching_event_ids: vec!["e1".into(), "e2".into()],
                    reason: "Runtime observed btnSave_Click → SqlCommand execution".into(),
                },
                PathReconciliation {
                    path_id: "path_002".into(),
                    status: ReconciliationStatus::Contradicted,
                    matching_event_ids: vec![],
                    reason: "Expected GridView binding but observed direct SQL call".into(),
                },
            ],
        };

        let json = serde_json::to_string_pretty(&result).unwrap();
        let decoded: ReconciliationResult = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.confirmed_count, 3);
        assert_eq!(decoded.details.len(), 2);
        assert_eq!(decoded.details[0].status, ReconciliationStatus::Confirmed);
    }
}
