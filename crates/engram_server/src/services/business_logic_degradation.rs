//! Safe provider diagnostics and bounded observations of supplied source.
use engram_ml::llm_provider::LlmError;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct AnalysisDegradation {
    pub reason: &'static str,
    pub status_code: Option<u16>,
    pub retry_exhausted: Option<bool>,
    pub deterministic_summary: Option<SourceSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceSummary {
    pub file_path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub content_hash: String,
    pub summary: String,
    pub observations: Vec<SourceObservation>,
    pub omitted_observations: usize,
    pub coverage: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceObservation {
    pub line: u32,
    pub kind: &'static str,
    pub source: String,
}

impl AnalysisDegradation {
    pub fn unavailable(reason: &'static str) -> Self {
        Self {
            reason,
            status_code: None,
            retry_exhausted: None,
            deterministic_summary: None,
        }
    }

    pub fn from_error(error: &LlmError) -> Self {
        let reason = match error {
            LlmError::Timeout { .. } => "timeout",
            LlmError::Auth { .. } => "authentication_failed",
            LlmError::RateLimited { .. } => "rate_limited",
            LlmError::InvalidResponse { .. } => "invalid_provider_response",
            LlmError::Upstream5xx { .. } => "upstream_failure",
            LlmError::Transport { .. } => "transport_failure",
            LlmError::Parse { .. } => "provider_parse_failure",
        };
        // Never expose the raw error message, URL or response body.
        Self {
            reason,
            status_code: error.status_code(),
            retry_exhausted: Some(error.retry_exhausted()),
            deterministic_summary: None,
        }
    }

    pub fn notice(&self) -> String {
        format!(
            "LLM analysis unavailable (reason={}, status={:?}, retry_exhausted={:?}); returning deterministic source observations only.",
            self.reason, self.status_code, self.retry_exhausted
        )
    }

    pub fn with_source(
        mut self,
        file: &str,
        source: &str,
        language: &str,
        start_line: u32,
    ) -> Self {
        let masked =
            super::super::business_outcome_dependencies::executable_lines(source, language == "vb");
        let mut observations = Vec::new();
        let mut total = 0usize;
        let mut returns = 0usize;
        let mut conditions = 0usize;
        let mut assignments = 0usize;
        // The existing masker retains line boundaries, excludes comments and
        // hides literals before these deliberately lexical observations.
        for (offset, (original, executable)) in source.lines().zip(masked.lines()).enumerate() {
            let line = executable.trim();
            if line.is_empty() {
                continue;
            }
            let lower = line.to_ascii_lowercase();
            let kind = if lower.starts_with("return ") || lower == "return" {
                returns += 1;
                "return_statement"
            } else if lower.starts_with("if ")
                || lower.starts_with("if(")
                || lower.starts_with("elseif ")
            {
                conditions += 1;
                "conditional_statement"
            } else if line.contains('=')
                && !line.contains("=>")
                && !line.contains("==")
                && !lower.starts_with("<")
            {
                assignments += 1;
                "assignment_or_comparison"
            } else {
                continue;
            };
            total += 1;
            // Whole lines only. Long lines and excess observations are counted,
            // never clipped into a misleading source citation.
            if observations.len() < 8 && original.len() <= 240 {
                observations.push(SourceObservation {
                    line: start_line.max(1).saturating_add(offset as u32),
                    kind,
                    source: original.to_owned(),
                });
            }
        }
        let shown = observations.len();
        self.deterministic_summary = Some(SourceSummary {
            file_path: file.into(),
            start_line: start_line.max(1),
            end_line: start_line
                .max(1)
                .saturating_add(source.lines().count().saturating_sub(1) as u32),
            content_hash: engram_core::ids::ContentHash::compute(source.as_bytes()).0,
            summary: format!(
                "Supplied source contains {returns} return, {conditions} conditional and {assignments} assignment/comparison line observations; showing {shown} of {total}."
            ),
            observations,
            omitted_observations: total - shown,
            coverage: "Bounded lexical source observations, not inferred business purpose/rules or proof of execution. Calls, multiline syntax, dispatch and effects are not exhaustively analyzed; zero observations does not mean no behavior.",
        });
        self
    }

    pub fn render(&self) -> String {
        let mut result = format!("\n**Degraded analysis**: {}\n", self.notice());
        if let Some(summary) = &self.deterministic_summary {
            result.push_str(&format!(
                "\n**Deterministic source summary**: {}\n\nSource: {}:{}; content hash: `{}`. {}\n",
                summary.summary,
                summary.file_path,
                summary.start_line,
                summary.content_hash,
                summary.coverage
            ));
            // JSON escapes source newlines, so authored fences cannot close
            // this report's code block. Statements remain complete strings.
            result.push_str("\n```json\n");
            result.push_str(
                &serde_json::to_string_pretty(&summary.observations)
                    .expect("source observation serialization"),
            );
            result.push_str("\n```\n");
            result.push('\n');
        }
        result
    }
}
