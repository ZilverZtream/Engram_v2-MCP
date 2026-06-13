//! TODO-28: `ask_codebase` — one natural-language front door.
//!
//! Heuristic intent routing onto the existing tool surface; no LLM in the
//! loop. The answer always names which tools were consulted so agents can
//! drill in directly next time.

use crate::tools::Engram;
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum Intent {
    /// "what breaks if/when I change|edit|remove X"
    Impact,
    /// "where is X used/called/referenced"
    Usage,
    /// "when/why did X change", "history of X"
    History,
    /// "as a/an ... I want", "add/implement <feature>"
    Feature,
    /// everything else: "how does X work", "what is X"
    Explain,
}

/// Classify a question and extract the subject phrase (best effort —
/// the routed tools do their own resolution/search on it).
pub(crate) fn classify(question: &str) -> (Intent, String) {
    let q = question.trim();
    let lower = q.to_lowercase();
    let strip = |s: &str, prefixes: &[&str]| -> String {
        let mut out = s.to_string();
        let mut low = out.to_lowercase();
        let mut changed = true;
        while changed {
            changed = false;
            for p in prefixes {
                if low.starts_with(p) {
                    out = out[p.len()..].trim().to_string();
                    low = out.to_lowercase();
                    changed = true;
                }
            }
        }
        out.trim_end_matches('?').trim().to_string()
    };

    if lower.starts_with("as a ")
        || lower.starts_with("as an ")
        || lower.starts_with("add ")
        || lower.starts_with("implement ")
        || lower.starts_with("create a ")
        || lower.starts_with("build ")
    {
        return (Intent::Feature, q.trim_end_matches('?').to_string());
    }
    if (lower.contains("what breaks")
        || lower.contains("blast radius")
        || lower.contains("impact of")
        || lower.contains("safe to"))
        || ((lower.starts_with("what happens") || lower.contains("risk"))
            && (lower.contains("change")
                || lower.contains("edit")
                || lower.contains("remove")
                || lower.contains("delete")
                || lower.contains("rename")))
    {
        let subject = strip(
            q,
            &[
                "what breaks if i change",
                "what breaks if i",
                "what breaks when",
                "what breaks if",
                "what is the blast radius of",
                "blast radius of",
                "what happens if i change",
                "what happens if i remove",
                "what happens if i",
                "impact of changing",
                "impact of",
                "is it safe to change",
                "is it safe to",
            ],
        );
        return (Intent::Impact, subject);
    }
    if lower.starts_with("where is")
        || lower.starts_with("where are")
        || lower.contains("who calls")
        || lower.contains("who uses")
        || lower.contains("used where")
        || lower.contains("references to")
    {
        let subject = strip(
            q,
            &[
                "where is",
                "where are",
                "who calls",
                "who uses",
                "references to",
            ],
        );
        let subject = subject
            .trim_end_matches("used")
            .trim_end_matches("called")
            .trim_end_matches("referenced")
            .trim()
            .to_string();
        return (Intent::Usage, subject);
    }
    if lower.starts_with("when did")
        || lower.starts_with("when was")
        || lower.starts_with("why did")
        || lower.starts_with("why was")
        || lower.contains("history of")
        || lower.contains("last changed")
    {
        let subject = strip(
            q,
            &["when did", "when was", "why did", "why was", "history of"],
        );
        let subject = subject
            .trim_end_matches("change")
            .trim_end_matches("changed")
            .trim()
            .to_string();
        return (Intent::History, subject);
    }
    let subject = strip(
        q,
        &[
            "how does",
            "how do",
            "how is",
            "what does",
            "what is",
            "explain",
            "show me",
            "tell me about",
        ],
    );
    let subject = subject.trim_end_matches("work").trim().to_string();
    (
        Intent::Explain,
        if subject.is_empty() {
            q.to_string()
        } else {
            subject
        },
    )
}

impl Engram {
    pub async fn handle_ask_codebase(
        &self,
        req: crate::models::AskCodebaseRequest,
    ) -> Result<CallToolResult, McpError> {
        crate::handlers::validate_project_id(&req.project_id)?;
        if req.question.trim().is_empty() {
            return Err(McpError::invalid_params(
                "question must not be empty".to_string(),
                None,
            ));
        }
        let (intent, subject) = classify(&req.question);
        let pid = req.project_id.clone();

        let text_of = |r: &CallToolResult| -> String {
            r.content
                .iter()
                .filter_map(|c| c.as_text().map(|t| t.text.clone()))
                .collect::<Vec<_>>()
                .join("\n")
        };

        let (consulted, body) = match intent {
            Intent::Feature => {
                let r = self
                    .handle_plan_user_story(crate::models::PlanUserStoryRequest {
                        project_id: pid,
                        story: req.question.clone(),
                        concepts: None,
                    })
                    .await?;
                ("plan_user_story", text_of(&r))
            }
            Intent::Impact => {
                let r = self
                    .handle_impact_analysis(crate::models::ImpactAnalysisRequest {
                        project_id: pid,
                        file_path: None,
                        symbol_fqn: Some(subject.clone()),
                        limit: 50,
                    })
                    .await?;
                ("impact_analysis (blast radius)", text_of(&r))
            }
            Intent::Usage => {
                let r = self
                    .handle_find_symbol_references(crate::models::FindSymbolReferencesRequest {
                        symbol_name: subject.clone(),
                        project_id: pid,
                        max_incoming: 25,
                        max_outgoing_per_kind: 10,
                        edge_kind_filter: None,
                        file_scope: None,
                    })
                    .await?;
                ("find_symbol_references", text_of(&r))
            }
            Intent::History => {
                let r = self
                    .handle_search_history(crate::models::SearchHistoryRequest {
                        query: subject.clone(),
                        project_id: pid,
                        file_filter: None,
                        exclude_paths: None,
                        author_filter: None,
                        date_after: None,
                        date_before: None,
                        limit: 8,
                        fts_mode: Default::default(),
                        max_content_chars: 600,
                        use_mmr: false,
                    })
                    .await?;
                ("search_history", text_of(&r))
            }
            Intent::Explain => {
                let search = self
                    .handle_search_memory(crate::models::SearchMemoryRequest {
                        query: subject.clone(),
                        project_id: pid.clone(),
                        max_results: 5,
                        ..Default::default()
                    })
                    .await?;
                let footprint = self
                    .handle_get_concept_footprint(crate::models::GetConceptFootprintRequest {
                        project_id: pid,
                        concept: subject.clone(),
                        max_per_group: 5,
                    })
                    .await
                    .map(|r| text_of(&r))
                    .unwrap_or_default();
                (
                    "search_memory + get_concept_footprint",
                    format!("{}\n\n{}", text_of(&search), footprint),
                )
            }
        };

        Ok(CallToolResult::success(vec![Content::text(format!(
            "# ask_codebase\nintent: {intent:?} | subject: \"{subject}\" | consulted: {consulted}\n\n{body}"
        ))]))
    }
}

#[cfg(test)]
mod tests {
    use super::{Intent, classify};

    #[test]
    fn routes_the_five_intents() {
        assert_eq!(
            classify("What breaks if I change SaveMarker?").0,
            Intent::Impact
        );
        assert_eq!(
            classify("where is ss_systemsettings used?").0,
            Intent::Usage
        );
        assert_eq!(classify("when did map.js last change?").0, Intent::History);
        assert_eq!(
            classify("As an admin I want to set minimum photos").0,
            Intent::Feature
        );
        assert_eq!(
            classify("how does marker clustering work?").0,
            Intent::Explain
        );
    }

    #[test]
    fn extracts_subjects() {
        let (_, s) = classify("What breaks if I change SaveMarker?");
        assert_eq!(s, "SaveMarker");
        let (_, s) = classify("where is ss_systemsettings used?");
        assert_eq!(s, "ss_systemsettings");
        let (_, s) = classify("how does marker clustering work?");
        assert_eq!(s, "marker clustering");
    }
}
