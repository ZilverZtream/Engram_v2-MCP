use regex::Regex;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::timeout;

#[derive(Debug, Clone)]
pub struct DreamInsight {
    pub title: String,
    pub summary_markdown: String,
    pub key_terms: Vec<String>,
}

#[derive(Clone, Default)]
pub struct DreamingEngine;

impl DreamingEngine {
    pub fn new() -> Self {
        Self
    }

    /// Public entry point with timeout and fallback logic.
    pub async fn summarize_cluster(
        &self,
        context_blobs: &[String],
        max_wait: Duration,
    ) -> DreamInsight {
        match timeout(max_wait, self.llm_summarize(context_blobs)).await {
            Ok(insight) => insight,
            Err(_) => {
                // Fallback to deterministic logic on timeout.
                self.deterministic_summarize(context_blobs)
            }
        }
    }

    async fn llm_summarize(&self, context_blobs: &[String]) -> DreamInsight {
        // Placeholder for future real LLM call (e.g. OpenAI/Ollama).
        // For now, just use the deterministic logic but wrapped in async.
        self.deterministic_summarize(context_blobs)
    }

    /// Deterministic, local "dream" summarizer.
    pub fn deterministic_summarize(&self, context_blobs: &[String]) -> DreamInsight {
        let re = Regex::new(r"[A-Za-z_][A-Za-z0-9_]{2,}").unwrap();
        let mut counts: HashMap<String, usize> = HashMap::new();

        for blob in context_blobs {
            for m in re.find_iter(blob) {
                let t = m.as_str();
                if t.len() > 40 {
                    continue;
                }
                // Downweight very common noise.
                if matches!(t, "self" | "this" | "that" | "None" | "Some" | "Result") {
                    continue;
                }
                *counts.entry(t.to_string()).or_insert(0) += 1;
            }
        }

        let mut top: Vec<(String, usize)> = counts.into_iter().collect();
        top.sort_by(|a, b| b.1.cmp(&a.1));
        top.truncate(8);

        let key_terms: Vec<String> = top.iter().map(|(t, _)| t.clone()).collect();
        let title = if key_terms.len() >= 2 {
            format!("Insight: {} ↔ {}", key_terms[0], key_terms[1])
        } else if let Some((t, _)) = top.first() {
            format!("Insight: {t}")
        } else {
            "Insight".into()
        };

        let mut summary = String::new();
        summary.push_str("### Why these areas seem linked (Deterministic Fallback)\n");
        summary.push_str(
            "- Repeated concepts and identifiers show up together across your recent context.\n",
        );

        if !key_terms.is_empty() {
            summary.push_str("- Shared signals: ");
            for (i, t) in key_terms.iter().enumerate() {
                if i > 0 {
                    summary.push_str(", ");
                }
                summary.push_str(&format!("`{t}`"));
            }
            summary.push_str(".\n");
        }

        summary.push_str("\n### Suggested follow-ups\n");
        summary.push_str("- Search for call sites / tests that mention the shared signals.\n");
        summary.push_str("- If this spans multiple modules, consider extracting an explicit interface boundary.\n");

        DreamInsight {
            title,
            summary_markdown: summary,
            key_terms,
        }
    }
}
