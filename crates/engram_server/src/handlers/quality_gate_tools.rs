//! Stage-3 quality-gate tools: ingest a project's accumulated "what to avoid"
//! knowledge (coding/agent rules, copilot-instructions.md, CodeRabbit &
//! SonarQube findings, the DevOps recurring-issues board) into a searchable
//! `quality_gate` namespace, and a pre-push audit that retrieves the rules
//! relevant to a proposed change so an agent can avoid repeating known mistakes
//! BEFORE the first push. This gives the agent the context a developer has but
//! the user story omits — the path beyond the story-alone parity cap.

use crate::handlers::validate_project_id;
use crate::tools::Engram;
use engram_core::safe_join;
use engram_index::HybridQuery;
use engram_index::quality_gates::{QualitySource, parse_quality_source};
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};

const QG_NAMESPACE: &str = "quality_gate";

impl Engram {
    /// Ingest a quality-gate source file into the `quality_gate` namespace.
    pub async fn handle_ingest_quality_gates(
        &self,
        req: crate::models::IngestQualityGatesRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let source = QualitySource::from_str(&req.source_type).ok_or_else(|| {
            McpError::invalid_params(
                format!(
                    "unknown source_type '{}'; expected one of: copilot, rules, coderabbit, \
                     sonarqube, board, text",
                    req.source_type
                ),
                None,
            )
        })?;

        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let active_gen = self.get_active_generation(&req.project_id).await?;

        let abs = safe_join(std::path::Path::new(&ps.info.directory), &req.source_path)
            .map_err(|e| McpError::invalid_request(e.to_string(), None))?;
        let content = std::fs::read_to_string(&abs)
            .map_err(|e| McpError::invalid_params(format!("cannot read source: {e}"), None))?;
        let origin = std::path::Path::new(&req.source_path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&req.source_path)
            .to_string();

        let rules = parse_quality_source(&content, source, &origin);
        if rules.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "No quality-gate rules parsed from {origin} (source_type={}).",
                req.source_type
            ))]));
        }

        let mut docs = Vec::with_capacity(rules.len());
        let mut by_sev = std::collections::BTreeMap::<String, usize>::new();
        for r in &rules {
            *by_sev.entry(r.severity.clone()).or_default() += 1;
            // Index the rule TEXT as content (clean for semantic match to code);
            // path carries the rule's file scope (or the source) so a per-file
            // audit can prefer scoped rules; language carries the category.
            let path = r.path_scope.clone().unwrap_or_else(|| origin.clone());
            let ch = engram_core::ContentHash::compute(r.text.as_bytes());
            let doc_id = engram_core::DocIdStr::compute(&path, 0, 0, &ch).0;
            docs.push(engram_index::IndexDoc {
                generation: active_gen,
                chunk_id: engram_index::chunk_id_from_content_hash(&ch),
                doc_id,
                content_hash: ch.0,
                path: engram_core::RelPath::new(&path),
                language: r.category.clone(),
                content: r.text.clone(),
                namespace: QG_NAMESPACE.to_string(),
                author: Some(r.severity.clone()),
                timestamp: None,
                start_line: 0,
                end_line: 0,
            });
        }

        let cancel = tokio_util::sync::CancellationToken::new();
        ps.search
            .index_docs(&req.project_id, &docs, &cancel)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let sev = by_sev
            .iter()
            .map(|(k, v)| format!("{v} {k}"))
            .collect::<Vec<_>>()
            .join(", ");
        Ok(CallToolResult::success(vec![Content::text(format!(
            "Ingested {} quality-gate rules from {origin} (source_type={}, category={}) into the \
             `{QG_NAMESPACE}` namespace [{sev}]. Use pre_push_audit to check a change against them.",
            rules.len(),
            req.source_type,
            rules[0].category,
        ))]))
    }

    /// Pre-push audit: retrieve the quality-gate rules most relevant to a
    /// proposed change so the agent can fix known issues before pushing.
    pub async fn handle_pre_push_audit(
        &self,
        req: crate::models::PrePushAuditRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        if req.code.trim().is_empty() {
            return Err(McpError::invalid_params("code must not be empty", None));
        }
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;
        let top_k = req.top_k.clamp(1, 50);

        let query = crate::utils::text::code_to_query(&req.code);
        let cancel = tokio_util::sync::CancellationToken::new();
        let hits = ps
            .search
            .search(
                &HybridQuery {
                    project_id: req.project_id.clone(),
                    namespace: QG_NAMESPACE.to_string(),
                    generation: gen_,
                    text: query,
                    top_k,
                    fts_mode: "loose".into(),
                    include_path_prefixes: None,
                    exclude_path_prefixes: None,
                    language_filters: None,
                    author_filter: None,
                    date_after: None,
                    date_before: None,
                    use_mmr: true,
                },
                None,
                &cancel,
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if hits.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "Pre-push audit: no quality-gate rules matched this change. (If you haven't run \
                 ingest_quality_gates for this project, there are no rules to check yet.)"
                    .to_string(),
            )]));
        }

        // Rules scoped to the edited file rank first.
        let scope = req.file_path.as_deref().map(|f| {
            f.replace('\\', "/")
                .rsplit('/')
                .next()
                .unwrap_or(f)
                .to_ascii_lowercase()
        });
        let mut out = String::from(
            "# Pre-push audit — verify the change against these known quality-gate rules\n\n",
        );
        out.push_str(
            "Each is a rule/finding the team has flagged before (coding rules, copilot-instructions, \
             CodeRabbit/SonarQube history, the recurring-issues board). Confirm your change does NOT \
             violate them before pushing.\n\n",
        );
        let mut scoped = Vec::new();
        let mut general = Vec::new();
        for h in &hits {
            let p = h.path.as_str().to_ascii_lowercase();
            let line = format!(
                "- [{}] {}",
                h.path.as_str(),
                h.snippet.as_deref().unwrap_or("").trim()
            );
            if scope.as_ref().is_some_and(|s| p.contains(s.as_str())) {
                scoped.push(line);
            } else {
                general.push(line);
            }
        }
        if !scoped.is_empty() {
            out.push_str("## Rules scoped to the file you're editing\n");
            out.push_str(&scoped.join("\n"));
            out.push_str("\n\n");
        }
        out.push_str("## Other relevant rules\n");
        out.push_str(&general.join("\n"));
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }
}
