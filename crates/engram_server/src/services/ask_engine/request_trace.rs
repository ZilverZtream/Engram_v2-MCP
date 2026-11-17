//! Compose the existing request tracer for explicit HTTP request mentions.
use super::{
    evidence::{Authority, EvidenceItem, EvidenceKind},
    status::{AnswerStatus, ProviderReport, ProviderStatus},
};
use regex::Regex;
use serde_json::{Value, json};
use std::sync::LazyLock;

pub const TRACE_TOKENS: usize = 16_384;
const INTERPRETATION: &str = "Static request candidates from the ordinary request tracer using the ask-held selected source inventory. Per-member source hash/revision checks, omissions, authored guidance and saved trace recovery remain in the nested report. This is not an atomic graph/filesystem snapshot or proof of HTTP hosting, dispatch, authorization, database execution or runtime behavior. Single-tool recovery runs a new trace against then-current state; saved ask-report recovery retains the original answer.";
static REQUESTS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)\b(GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS)\s+((?:https?://|~/|/)[^\s"'`<>]+)"#,
    )
    .expect("explicit HTTP request mentions")
});

/// Retain a third mention to disclose the two-request selection cap.
pub fn queries(question: &str) -> Vec<String> {
    let mut out = Vec::new();
    for capture in REQUESTS.captures_iter(question) {
        let url = capture[2].trim_end_matches([',', ';', ')', ']', '}']);
        let request = format!("{} {url}", capture[1].to_ascii_uppercase());
        if !out.contains(&request) {
            out.push(request);
        }
        if out.len() == 3 {
            break;
        }
    }
    out
}

pub struct RequestEvidence {
    pub items: Vec<EvidenceItem>,
    pub provider: ProviderReport,
    pub matched_names: Vec<String>,
}
impl RequestEvidence {
    pub fn new(available: usize) -> Self {
        Self {
            items: Vec::new(),
            matched_names: Vec::new(),
            provider: ProviderReport {
                provider: "request_trace".into(),
                status: ProviderStatus::Empty,
                count: 0,
                note: Some(INTERPRETATION.into()),
                examined: 0,
                available: Some(available),
                truncated: available > 2,
                proof: None,
            },
        }
    }
    pub fn note(&mut self, note: &str) {
        let previous = self.provider.note.take().unwrap_or_default();
        self.provider.note = Some(format!("{previous} {note}"));
    }
    pub fn qualify_status(&self, status: AnswerStatus) -> AnswerStatus {
        match status {
            AnswerStatus::Answered => AnswerStatus::Partial,
            other => other,
        }
    }
    pub fn retain(
        &mut self,
        project: &str,
        entry: &str,
        generation: u64,
        trace: Value,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            trace["active_generation"].as_u64() == Some(generation),
            "Request trace selection differs from the held ask generation"
        );
        anyhow::ensure!(
            trace["status"] == "static_candidates",
            "Request tracer returned an unexpected report kind"
        );
        let pages = trace["routing"]["candidates"]
            .as_array()
            .map_or(0, Vec::len);
        let apis = trace["routing"]["api_candidates"]
            .as_array()
            .map_or(0, Vec::len);
        if pages + apis == 0 {
            self.note(&format!("No static routing candidate was returned for {entry}; this is not proof that a runtime route is absent."));
            return Ok(());
        }
        let mut recovery = json!({"project_id":project,"entry_point":entry,"include_db":true,
            "include_triggers":true,"max_tokens":TRACE_TOKENS});
        if let Some(scopes) = trace["coverage"]["path_prefixes"].as_array().filter(|scopes| !scopes.is_empty()) {
            recovery["path_prefixes"] = json!(scopes);
        }
        self.items.push(EvidenceItem {
            evidence_id: format!("request_trace_{}", self.items.len()+1),
            document_id: None, document_namespace: Some("memory".into()),
            source_verification: Some("HELD_SELECTED_SOURCE_REQUEST_TRACE: nested per-member raw hash and physical revision checks".into()),
            kind: EvidenceKind::GraphRelation, authority: Authority::CurrentCode,
            // A trace spans markup, handlers and dependencies. Its identity is
            // the HTTP request; every actual file/range stays in the report.
            path: None, lines: None, symbol_id: Some(format!("request_trace:{entry}")),
            title: Some(format!("{entry}: static request trace candidates")),
            content: json!({"evidence_kind":"http_request_trace_candidates","entry_point":entry,
                "trace":trace,"recovery":{"tool":"trace_user_request","arguments":recovery},
                "interpretation":INTERPRETATION}).to_string(),
            generation: Some(generation), commit: None, timestamp: None,
            confidence: 0.9, relevance: 1.0, extraction_method: "selected_source_request_trace".into(),
            warnings: vec![INTERPRETATION.into()], provider: "request_trace".into(),
            score: None, directness: Some(0.9),
        });
        self.matched_names.push(entry.into());
        self.provider.count = self.items.len();
        self.provider.status = ProviderStatus::Hit;
        Ok(())
    }
}
