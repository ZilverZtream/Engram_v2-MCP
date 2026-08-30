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
use engram_index::quality_gates::{
    QualityRule, QualitySource, batch_findings, distill_prompt, parse_distilled_rules,
    parse_quality_source,
};
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use std::sync::Arc;
use std::time::Duration;

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
        // Row-3 audit A7: `clear_existing` was a documented no-op. It purges the
        // project's whole `quality_gate` namespace — only after the new source
        // parsed, so a bad file never wipes a good corpus — and states it.
        let purged = if req.clear_existing {
            match ps
                .search
                .delete_namespace(&req.project_id, QG_NAMESPACE)
                .await
            {
                Ok(n) => Some(n),
                Err(e) => {
                    return Err(McpError::internal_error(
                        format!(
                            "clear_existing: purging the `{QG_NAMESPACE}` namespace failed: {e}"
                        ),
                        None,
                    ));
                }
            }
        } else {
            None
        };
        {}

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

        // BRIDGE: guideline-class mandates also become REGISTRY repo rules —
        // the store the pre-commit gates and get_chunk rule-injection read.
        // Without this, a team guideline (copilot-instructions demanding XML
        // docs) lives only in the searchable namespace and every gate that
        // keys on ctx.repo_rules stays blind to it (live gap: gate 17's
        // docs check needed a manual add_repo_rule). Promotion is bounded
        // and shape-gated: high-severity rules whose text reads as a
        // mandate, deduped against existing rules by prefix.
        let mut promoted = 0usize;
        {
            let existing: Vec<String> = self
                .state
                .registry
                .list_repo_rules(&req.project_id)
                .unwrap_or_default()
                .into_iter()
                .map(|r| r.rule_text.to_lowercase())
                .collect();
            let is_mandate = |t: &str| {
                let l = t.to_lowercase();
                ["must", "never", "always", "required", "do not", "don't"]
                    .iter()
                    .any(|k| l.contains(k))
            };
            // Guideline sources (copilot-instructions, coding rules, and
            // the DevOps recurring-issues/learnings board) are team
            // mandates by nature — their parsers assign a flat "medium"
            // severity, so severity only gates the FINDING sources
            // (coderabbit/sonarqube exports), where it is real. The board
            // is mandate-heavy ("Make sure all .js are transpiled…",
            // "Must be tested on a real phone…") and belongs here.
            let guideline_source = matches!(
                source,
                QualitySource::CopilotInstructions
                    | QualitySource::CodingRulesMd
                    | QualitySource::DevOpsBoard
            );
            for r in rules.iter().filter(|r| {
                (guideline_source || r.severity.eq_ignore_ascii_case("high"))
                    && is_mandate(&r.text)
                    && r.text.len() >= 30
                    && r.text.len() <= 400
            }) {
                if promoted >= 20 {
                    break;
                }
                let key: String = r.text.to_lowercase().chars().take(80).collect();
                if existing.iter().any(|e| e.starts_with(&key)) {
                    continue;
                }
                let rule = engram_core::registry::RepoRule {
                    rule_id: format!("qg-{}", &r.id),
                    file_pattern: r.path_scope.clone().unwrap_or_else(|| "**/*".into()),
                    rule_text: r.text.clone(),
                    priority: 5,
                    updated_at_ms: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0),
                };
                if self
                    .state
                    .registry
                    .put_repo_rule(&req.project_id, &rule)
                    .is_ok()
                {
                    promoted += 1;
                }
            }
        }

        let sev = by_sev
            .iter()
            .map(|(k, v)| format!("{v} {k}"))
            .collect::<Vec<_>>()
            .join(", ");
        let purge_line = match purged {
            Some(n) => format!(
                "clear_existing=true: purged {n} existing quality-gate rule(s) from the \
                 `{QG_NAMESPACE}` namespace before ingesting.\n"
            ),
            None => String::new(),
        };
        Ok(CallToolResult::success(vec![Content::text(format!(
            "{purge_line}Ingested {} quality-gate rules from {origin} (source_type={}, category={}) into the \
             `{QG_NAMESPACE}` namespace [{sev}]. {promoted} high-severity mandate(s) auto-promoted \
             to repo rules (gates + rule injection read those). Use pre_push_audit to check a \
             change against them.",
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
        // Row-3 audit A6: the mandated pre-push step must say what it
        // checked. An empty namespace means it checked NOTHING.
        let (rules_total, count_failure): (usize, Option<String>) =
            match ps.search.count_docs_by_namespace(&req.project_id) {
                Ok(m) => (m.get(QG_NAMESPACE).copied().unwrap_or(0), None),
                Err(e) => (0, Some(format!("quality-gate rule count failed: {e}"))),
            };
        if rules_total == 0 && count_failure.is_none() {
            return Ok(CallToolResult::success(vec![Content::text(
                "Pre-push audit: INACTIVE — 0 quality-gate rules are ingested for this project, so \
                 NOTHING was checked. Run ingest_quality_gates (DevOps rules / copilot-instructions / \
                 CodeRabbit history) and re-run; until then this step is not evidence."
                    .to_string(),
            )]));
        }
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
                    include_path_suffixes: None,
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
            let mut msg = format!(
                "Pre-push audit: no quality-gate rules matched this change — {rules_total} rule(s) \
                 exist in the `{QG_NAMESPACE}` namespace and were searched (top_k {top_k}); 0 checked \
                 against this code."
            );
            if let Some(f) = &count_failure {
                msg.push_str(&format!("\nFAILURE: {f}"));
            }
            return Ok(CallToolResult::success(vec![Content::text(msg)]));
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
        out.push_str(&format!(
            "Checked: {} rule(s) retrieved of {} in the namespace (top_k {}{})\n\n",
            hits.len(),
            rules_total,
            top_k,
            if hits.len() >= top_k {
                " — the cap was filled; raise top_k for more"
            } else {
                ""
            }
        ));
        if let Some(f) = &count_failure {
            out.push_str(&format!("FAILURE: {f}\n\n"));
        }
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

    /// Distill a raw finding corpus (CodeRabbit/Sonar history) into GENERIC,
    /// deduplicated project rules, then store them in the `quality_gate`
    /// namespace. Unlike `ingest_quality_gates` (which stores findings 1:1, so a
    /// per-file audit can surface a file's own past finding), distillation
    /// GENERALIZES: it batches the thousands of file/line-specific findings and
    /// LLM-summarizes each batch into reusable rules that apply to ANY change
    /// ("the team keeps shipping un-parameterized SQL" -> "always parameterize").
    /// This is the team knowledge a developer carries in their head — the context
    /// a user story omits — and the right thing to feed planners / a pre-push
    /// audit. Use this for coderabbit/sonarqube corpora; use ingest for already-
    /// generic sources (copilot-instructions, the board).
    pub async fn handle_distill_quality_gates(
        &self,
        req: crate::models::DistillQualityGatesRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let source = QualitySource::from_str(&req.source_type).ok_or_else(|| {
            McpError::invalid_params(
                format!(
                    "unknown source_type '{}'; expected one of: coderabbit, sonarqube, board, \
                     text, copilot, rules",
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

        let findings = parse_quality_source(&content, source, &origin);
        if findings.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "No findings parsed from {origin} (source_type={}).",
                req.source_type
            ))]));
        }
        let n_findings = findings.len();

        // Build per-batch prompts up front (owned), so the spawned LLM tasks
        // don't borrow `findings`.
        let batch_size = req.batch_size.clamp(10, 200);
        let prompts: Vec<String> = batch_findings(&findings, batch_size)
            .iter()
            .map(|b| distill_prompt(b))
            .collect();
        let n_batches = prompts.len();

        // Distill each batch via the configured LLM, bounded concurrency.
        if self.state.dreaming.is_degraded() {
            return Err(McpError::internal_error(
                "distillation needs an LLM but the configured backend is unavailable (degraded \
                 mode); set a valid llm_provider/llm_model in the config."
                    .to_string(),
                None,
            ));
        }
        let dreaming = self.state.dreaming.clone();
        let sem = Arc::new(tokio::sync::Semaphore::new(req.max_concurrent.clamp(1, 12)));
        let mut handles = Vec::with_capacity(prompts.len());
        for prompt in prompts {
            let d = dreaming.clone();
            let s = sem.clone();
            let origin = origin.clone();
            handles.push(tokio::spawn(async move {
                let _permit = s.acquire().await.ok()?;
                match d
                    .generate_text(&prompt, 2000, Duration::from_secs(120))
                    .await
                {
                    Ok(raw) => Some(parse_distilled_rules(&raw, &origin)),
                    Err(e) => {
                        tracing::warn!("distill batch LLM call failed: {e:#}");
                        None
                    }
                }
            }));
        }

        let mut candidates: Vec<QualityRule> = Vec::new();
        let mut failed_batches = 0usize;
        for h in handles {
            match h.await {
                Ok(Some(rules)) => candidates.extend(rules),
                _ => failed_batches += 1,
            }
        }

        // Deterministic cross-batch dedup: collapse rules whose normalized text
        // prefix matches (topical batching keeps cross-batch dupes few).
        let mut seen = std::collections::HashSet::new();
        let mut rules: Vec<QualityRule> = Vec::new();
        for r in candidates {
            let key: String = r
                .text
                .to_ascii_lowercase()
                .chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .take(80)
                .collect();
            if seen.insert(key) {
                rules.push(r);
            }
        }

        if rules.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "Distilled 0 rules from {n_findings} findings in {origin} ({n_batches} batches, \
                 {failed_batches} LLM failures). The LLM returned no parseable rules."
            ))]));
        }

        // Store the generic rules in the quality_gate namespace.
        let mut docs = Vec::with_capacity(rules.len());
        let mut by_sev = std::collections::BTreeMap::<String, usize>::new();
        for r in &rules {
            *by_sev.entry(r.severity.clone()).or_default() += 1;
            let path = r.path_scope.clone().unwrap_or_else(|| origin.clone());
            let ch = engram_core::ContentHash::compute(r.text.as_bytes());
            let doc_id = engram_core::DocIdStr::compute(&path, 0, 0, &ch).0;
            docs.push(engram_index::IndexDoc {
                generation: active_gen,
                chunk_id: engram_index::chunk_id_from_content_hash(&ch),
                doc_id,
                content_hash: ch.0,
                path: engram_core::RelPath::new(&path),
                language: format!("distilled:{}", r.category),
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
            "Distilled {n_findings} raw findings from {origin} into {} GENERIC rules \
             ({n_batches} batches, {failed_batches} LLM failures) and indexed them into the \
             `{QG_NAMESPACE}` namespace [{sev}]. These apply to ANY change (not file/line lookup); \
             pre_push_audit and the planners can now retrieve them.",
            rules.len(),
        ))]))
    }
}
