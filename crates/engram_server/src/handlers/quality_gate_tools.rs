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
    parse_quality_source_checked, MARKDOWN_CONTEXT_PREFIX,
};
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use std::sync::Arc;
use std::time::Duration;
use crate::services::quality_source::{self, SourceReceipt};

const QG_NAMESPACE: &str = "quality_gate";

/// The import and lifecycle preflight share precisely the same promotion gate.
pub(crate) const SOURCE_PROMOTION_LIMIT: usize = 20;
pub(crate) fn source_promotion_candidate(rule: &QualityRule, source: QualitySource) -> bool {
    let guideline = matches!(source, QualitySource::CopilotInstructions
        | QualitySource::CodingRulesMd | QualitySource::DevOpsBoard);
    let lower = rule.text.to_lowercase();
    !rule.text.starts_with(MARKDOWN_CONTEXT_PREFIX)
        && (guideline || rule.severity.eq_ignore_ascii_case("high"))
        && ["must","never","always","required","do not","don't"].iter().any(|word| lower.contains(word))
        && rule.text.len() >= 30 && rule.text.len() <= 400
}

fn quality_source_origin(root: &std::path::Path, source: &std::path::Path) -> Result<String, McpError> {
    crate::services::legacy_rule_adoption::source_origin(root, source)
        .map_err(|e| McpError::invalid_params(format!(
            "Cannot resolve quality-gate source '{}': {e}. No rules imported.", source.display()
        ), None))
}

/// Resolve one input before quality-rule count/search. File identity uses existing BLAKE3 dependency.
fn resolve_audit_input(req: &crate::models::PrePushAuditRequest, root: &std::path::Path) -> Result<String, McpError> {
    use std::io::Read;
    match (&req.diff_file, &req.diff_file_blake3) {
        (None,None) if !req.code.trim().is_empty() => Ok(req.code.clone()),
        (Some(path),Some(expected)) if req.code.is_empty() => {
            if expected.len()!=64 || !expected.bytes().all(|b|b.is_ascii_hexdigit()) {
                return Err(McpError::invalid_params("diff_file_blake3 must be64 hex characters",None));
            }
            let path=safe_join(root,path).map_err(|e|McpError::invalid_params(e.to_string(),None))?;
            let file=std::fs::File::open(&path).map_err(|e|McpError::invalid_params(format!("cannot read diff_file: {e}"),None))?;
            if !file.metadata().map_err(|e|McpError::invalid_params(e.to_string(),None))?.is_file() {
                return Err(McpError::invalid_params("diff_file must be a regular file",None));
            }
            const LIMIT:u64=4*1024*1024;
            let mut bytes=Vec::new();file.take(LIMIT+1).read_to_end(&mut bytes).map_err(|e|McpError::invalid_params(e.to_string(),None))?;
            if bytes.len() as u64>LIMIT { return Err(McpError::invalid_params("diff_file exceeds4MiB",None)); }
            if !blake3::hash(&bytes).to_hex().as_str().eq_ignore_ascii_case(expected) {
                return Err(McpError::invalid_params("diff_file BLAKE3 mismatch",None));
            }
            let code=String::from_utf8(bytes).map_err(|_|McpError::invalid_params("diff_file must be UTF-8; no lossy identity conversion",None))?;
            if code.trim().is_empty() { return Err(McpError::invalid_params("diff_file must not be empty",None)); }
            Ok(code)
        },
        _ => Err(McpError::invalid_params("Provide nonblank code OR diff_file+diff_file_blake3; inputs must not be mixed or incomplete",None)),
    }
}

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

        if let Some(review) = &req.alias_review {
            if req.adoption.is_some() {
                return Err(McpError::invalid_params("alias_review and adoption are mutually exclusive", None));
            }
            let guard = if matches!(review.mode, crate::models::LegacyRuleAdoptionMode::Apply) {
                Some(self.state.acquire_project_update_lock(&req.project_id).await)
            } else { None };
            let state = self.state.clone();
            let report = tokio::task::spawn_blocking(move || {
                let _guard = guard;
                crate::services::legacy_rule_alias_review::execute(&state, &req, source)
            }).await.map_err(|e| McpError::internal_error(e.to_string(), None))?
                .map_err(|e| McpError::invalid_request(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(report)]));
        }

        if let Some(adoption) = &req.adoption {
            let guard = if matches!(adoption.mode, crate::models::LegacyRuleAdoptionMode::Apply) {
                Some(self.state.acquire_project_update_lock(&req.project_id).await)
            } else { None };
            let state = self.state.clone();
            let report = tokio::task::spawn_blocking(move || {
                let _guard = guard;
                crate::services::legacy_rule_adoption::execute(&state, &req, source)
            })
                .await.map_err(|e| McpError::internal_error(e.to_string(), None))?
                .map_err(|e| McpError::invalid_request(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(report)]));
        }

        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let _import_guard = self.state.acquire_project_update_lock(&req.project_id).await;
        let active_gen = self.get_active_generation(&req.project_id).await?;

        let abs = safe_join(std::path::Path::new(&ps.info.directory), &req.source_path)
            .map_err(|e| McpError::invalid_request(e.to_string(), None))?;
        let root = std::path::Path::new(&ps.info.directory);
        let origin = quality_source_origin(root, &abs)?;
        let content = crate::services::legacy_rule_adoption::read_source(root, &origin)
            .map_err(|e| McpError::invalid_params(format!("cannot read source: {e}"), None))?;

        let rules = parse_quality_source_checked(&content, source, &origin)
            .map_err(|error| McpError::invalid_params(error, None))?;
        let receipt_key = quality_source::receipt_key(&origin);
        let previous_raw = self.state.registry.get_meta_bounded(&req.project_id, &receipt_key, 4 * 1024 * 1024)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let previous = quality_source::receipt(&self.state, &req.project_id, &origin)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let authored = matches!(source, QualitySource::CopilotInstructions | QualitySource::CodingRulesMd);
        if previous.as_ref().is_some_and(|r| r.authored != authored) {
            return Err(McpError::invalid_params("Cannot change a source between authored policy and historical findings; review source ownership first. No rules imported.", None));
        }
        let replace = authored || req.replace_source;
        if rules.is_empty() && previous.is_none() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "No quality-gate rules parsed from {origin} (source_type={}).",
                req.source_type
            ))]));
        }
        let existing = self.state.registry.list_repo_rules(&req.project_id)
            .map_err(|error| McpError::internal_error(format!("cannot inspect existing repo rules: {error}"), None))?;
        if let Some(previous) = &previous {
            for owned in &previous.rules {
                if owned.provenance.as_deref() != Some(format!("quality-gate source: {origin}").as_str()) {
                    return Err(McpError::invalid_params(format!(
                        "Legacy receipt claim `{}` requires explicit ownership adoption before refreshing `{origin}`. No rules imported.", owned.rule_id
                    ), None));
                }
                if !existing.iter().any(|current| quality_source::same_rule(owned, current)) {
                    return Err(McpError::invalid_params(format!(
                        "Source-owned rule `{}` was manually changed or removed. Resolve ownership before refreshing `{origin}`. No rules imported.", owned.rule_id
                    ), None));
                }
            }
        }
        let provenance = format!("quality-gate source: {origin}");
        let basename = abs.file_name().and_then(|name| name.to_str()).unwrap_or(&origin);
        // Older promotions recorded only a basename. Their owning source cannot
        // be inferred when importing a nested file with the same name.
        for rule in &rules {
            let id = format!("qg-{}", rule.id);
            let legacy = id.replacen(&format!(":{origin}:"), &format!(":{basename}:"), 1);
            if origin != basename && existing.iter().any(|old| old.rule_id == legacy
                && old.provenance.as_deref() == Some("quality-gate finding")) {
                return Err(McpError::invalid_params(format!(
                    "Cannot establish ownership of legacy basename-only promotion `{legacy}` while importing `{origin}`. Review that repository rule's source before migrating it; clearing the searchable corpus does not resolve ownership. No rules imported."
                ), None));
            }
        }
        let aliases = crate::services::legacy_rule_adoption::resolve_aliases(&rules, previous.as_ref())
            .map_err(|e| McpError::invalid_params(format!("{e}. No rules imported."), None))?;
        let mut docs = Vec::with_capacity(rules.len());
        let mut by_sev = std::collections::BTreeMap::<String, usize>::new();
        let source_hash = blake3::hash(content.as_bytes()).to_hex().to_string();
        for r in &rules {
            *by_sev.entry(r.severity.clone()).or_default() += 1;
            // Index the rule TEXT as content (clean for semantic match to code);
            // path carries the rule's file scope (or the source) so a per-file
            // audit can prefer scoped rules; language carries the category.
            let path = r.path_scope.clone().unwrap_or_else(|| origin.clone());
            let indexed_text = format!(
                "Quality-rule source: {origin}\nSource BLAKE3: {source_hash}\nRepository-rule pattern: {}\n\n{}",
                r.path_scope.as_deref().unwrap_or("none supplied"), r.text,
            );
            let ch = engram_core::ContentHash::compute(indexed_text.as_bytes());
            let doc_id = engram_core::DocIdStr::compute(&path, 0, 0, &ch).0;
            docs.push(engram_index::IndexDoc {
                generation: active_gen,
                chunk_id: engram_index::chunk_id_from_content_hash(&ch),
                doc_id,
                content_hash: ch.0,
                path: engram_core::RelPath::new(&path),
                language: r.category.clone(),
                content: indexed_text,
                namespace: QG_NAMESPACE.to_string(),
                author: Some(r.severity.clone()),
                timestamp: None,
                start_line: 0,
                end_line: 0,
            });
        }

        // BRIDGE: guideline-class mandates also become REGISTRY repo rules —
        // the store the pre-commit gates and get_chunk rule-injection read.
        // Without this, a team guideline (copilot-instructions demanding XML
        // docs) lives only in the searchable namespace and every gate that
        // keys on ctx.repo_rules stays blind to it (live gap: gate 17's
        // docs check needed a manual add_repo_rule). Promotion is bounded
        // and shape-gated: high-severity rules whose text reads as a
        // mandate, deduped against existing rules by prefix.
        // Markdown candidates retain examples and exceptions. Their keyword matches
        // are retrieval leads, not independently scoped enforceable constraints.
        let contextual = rules.iter().filter(|r| r.text.starts_with(MARKDOWN_CONTEXT_PREFIX)).count();
        let mut promoted = 0usize;
        let mut promotions: Vec<_> = aliases.values().cloned().collect();
        {
            for r in rules.iter().filter(|r| {
                !aliases.contains_key(&r.id) && source_promotion_candidate(r, source)
            }) {
                if promoted >= SOURCE_PROMOTION_LIMIT {
                    break;
                }
                let rule_id = format!("qg-{}", &r.id);
                let file_pattern = r.path_scope.clone().unwrap_or_else(|| "**/*".into());
                // Separate source-owned IDs avoid cross-source reference counting:
                // retiring this policy must not disable another area's mandate.
                let mut rule = engram_core::registry::RepoRule {
                    rule_id,
                    file_pattern,
                    rule_text: r.text.clone(),
                    priority: 5,
                    updated_at_ms: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0),
                    introduced_at: None,
                    provenance: Some(provenance.clone()),
                };
                // A no-op source refresh must retain the rule's original dates.
                if let Some(old) = previous.as_ref().and_then(|p| p.rules.iter().find(|old| old.rule_id == rule.rule_id)) {
                    rule.introduced_at = old.introduced_at.clone();
                    let mut unchanged = rule.clone();
                    unchanged.updated_at_ms = old.updated_at_ms;
                    if unchanged == *old { rule = unchanged; }
                }
                promoted += 1;
                promotions.push(rule);
            }
        }

        let mut resolved_ids = std::collections::BTreeSet::new();
        for rule in &promotions {
            if !resolved_ids.insert(&rule.rule_id) {
                return Err(McpError::invalid_params(format!(
                    "Resolved promotion ID `{}` collides with another source entry; review alias mappings. No rules imported.", rule.rule_id
                ), None));
            }
            if existing.iter().any(|old| old.rule_id == rule.rule_id)
                && !previous.as_ref().is_some_and(|p| p.rules.iter().any(|old| old.rule_id == rule.rule_id
                    && old.provenance.as_deref() == Some(provenance.as_str()))) {
                return Err(McpError::invalid_params(format!(
                    "Promotion ID `{}` is owned by another repository rule or has no ownership receipt; explicit adoption is required. No rules imported.", rule.rule_id
                ), None));
            }
        }

        let mut owned_docs = previous.as_ref().map(|r| r.doc_ids.clone()).unwrap_or_default();
        if let Some(r) = &previous { owned_docs.extend(r.cleanup.iter().cloned()); }
        owned_docs.extend(docs.iter().map(|d| d.doc_id.clone()));
        owned_docs.sort(); owned_docs.dedup();
        let pending = SourceReceipt {
            origin: origin.clone(), source_hash: source_hash.clone(), authored, pending: true,
            doc_ids: owned_docs.clone(), rules: previous.as_ref().map(|r| r.rules.clone()).unwrap_or_default(),
            cleanup: Vec::new(),
            legacy_aliases: previous.as_ref().map(|r| r.legacy_aliases.clone()).unwrap_or_default(),
            corpus_revision: self.state.registry.get_meta(&req.project_id, quality_source::CORPUS_REVISION)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?.unwrap_or_default(),
        };
        let pending_raw = pending.encode().map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let saved = self.state.registry.compare_exchange_meta(&req.project_id, &receipt_key, previous_raw.as_deref(), &pending_raw)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        if !saved { return Err(McpError::invalid_request("Source receipt changed; retry import", None)); }
        // All failures after this point leave an explicit pending receipt. Managed
        // mandates are withheld until a retry completes publication atomically.
        let partial = |e: String| McpError::internal_error(format!(
            "Quality-source refresh incomplete: {e}. Source-managed constraints are withheld; retry this import."
        ), None);
        let corpus_revision = if req.clear_existing {
            let revision = uuid::Uuid::new_v4().to_string();
            self.state.registry.set_meta(&req.project_id, quality_source::CORPUS_REVISION, &revision)
                .map_err(|e| partial(e.to_string()))?;
            revision
        } else { pending.corpus_revision.clone() };
        let purged = if req.clear_existing {
            Some(ps.search.delete_namespace(&req.project_id, QG_NAMESPACE).await.map_err(|e| partial(format!("search corpus clear: {e:#}")))?)
        } else { None };
        let cancel = tokio_util::sync::CancellationToken::new();
        if !docs.is_empty() {
            ps.search.index_docs(&req.project_id, &docs, &cancel).await.map_err(|e| partial(format!("replacement search indexing: {e:#}")))?;
        }
        let reread = crate::services::legacy_rule_adoption::read_source(root, &origin).map_err(|e| partial(e.to_string()))?;
        if blake3::hash(reread.as_bytes()).to_hex().as_str() != source_hash {
            return Err(partial("source changed during import; publication aborted".into()));
        }
        let current_docs: Vec<String> = if replace || req.clear_existing {
            docs.iter().map(|d| d.doc_id.clone()).collect()
        } else { owned_docs.clone() };
        let obsolete: Vec<String> = owned_docs.into_iter().filter(|id| !current_docs.contains(id)).collect();
        let mut published_rules = if replace { Vec::new() } else {
            previous.as_ref().map(|r| r.rules.clone()).unwrap_or_default()
        };
        published_rules.retain(|old| !promotions.iter().any(|new| new.rule_id == old.rule_id));
        published_rules.extend(promotions.iter().cloned());
        let remove: Vec<String> = if replace {
            previous.as_ref().map(|r| r.rules.iter().map(|rule| rule.rule_id.clone()).collect()).unwrap_or_default()
        } else { Vec::new() };
        let mut published = SourceReceipt { origin: origin.clone(), source_hash, authored, pending: false,
            doc_ids: current_docs, rules: published_rules, cleanup: obsolete.clone(), corpus_revision,
            legacy_aliases: previous.as_ref().map(|r| r.legacy_aliases.clone()).unwrap_or_default() };
        let published_raw = published.encode().map_err(|e| partial(e.to_string()))?;
        self.state.registry.publish_repo_rule_source(&req.project_id, &existing, &remove, &promotions,
            &receipt_key, &pending_raw, &published_raw).map_err(|e| partial(e.to_string()))?;
        let cleanup_note = match ps.search.delete_documents(&req.project_id, QG_NAMESPACE, &obsolete).await {
            Ok(()) => {
                published.cleanup.clear();
                let clean_raw = published.encode().map_err(|e| partial(e.to_string()))?;
                match self.state.registry.compare_exchange_meta(&req.project_id, &receipt_key, Some(&published_raw), &clean_raw) {
                    Ok(true) => String::new(),
                    _ => " Cleanup receipt not finalized; retry import to reconcile. ".into(),
                }
            },
            Err(error) => format!(" Obsolete evidence is logically retired; physical cleanup pending: {error}. Retry import. "),
        };

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
             `{QG_NAMESPACE}` namespace [{sev}]. {promoted} qualifying mandate(s) auto-promoted \
             to repo rules (gates + rule injection read those). Use pre_push_audit to check a \
             change against them. Source replacement={replace}; {} obsolete owned document(s) retired.{cleanup_note} {contextual} context-required Markdown candidate(s) retained for full-source applicability review; these were not auto-promoted to enforceable repo rules.",
            rules.len(),
            req.source_type,
            rules.first().map(|r| r.category.as_str()).unwrap_or("empty source"),
            obsolete.len(),
        ))]))
    }

    /// Pre-push audit: retrieve the quality-gate rules most relevant to a
    /// proposed change so the agent can fix known issues before pushing.
    pub async fn handle_pre_push_audit(
        &self,
        req: crate::models::PrePushAuditRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        if let Some(maintenance) = req.instruction_maintenance.as_ref() {
            if !req.code.is_empty() || req.diff_file.is_some() || req.diff_file_blake3.is_some() || req.file_path.is_some() {
                return Err(McpError::invalid_params("Maintenance and retrieval inputs must not be mixed", None));
            }
            let record = self.ensure_project_record(&req.project_id).await?;
            let project_id = req.project_id.clone();
            let maintenance = maintenance.clone();
            let assembly = if maintenance.area_context.is_some() {
                Some(crate::services::instruction_maintenance::area_evidence::assemble(
                    &self.state, &project_id, std::path::Path::new(&record.directory), maintenance.clone(),
                ).await.map_err(|error| McpError::invalid_params(format!("Area evidence unavailable: {error:#}"), None))?)
            } else { None };
            let report = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
                let report = match &assembly {
                    Some(assembly) => crate::services::instruction_maintenance::review_with_area(
                        std::path::Path::new(&record.directory), &project_id, &assembly.request, Some(&assembly.evidence),
                    )?,
                    None => crate::services::instruction_maintenance::review(
                        std::path::Path::new(&record.directory), &project_id, &maintenance,
                    )?,
                };
                // The assembled source lease survives all collection and serialization.
                Ok(serde_json::to_string_pretty(&report)?)
            }).await.map_err(|e| McpError::internal_error(e.to_string(), None))?
                .map_err(|e| McpError::invalid_params(format!("Instruction maintenance unavailable: {e:#}"), None))?;
            return Ok(CallToolResult::success(vec![Content::text(report)]));
        }
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let code = resolve_audit_input(&req, std::path::Path::new(&ps.info.directory))?;
        let mut input_binding = format!("Input: {} bytes, BLAKE3 {} ({}).\n",code.len(),blake3::hash(code.as_bytes()).to_hex(),if req.diff_file.is_some(){"project-relative file"}else{"inline"});
        let audit_paths = req.file_path.as_deref().map(|path| [path]);
        match quality_source::effective_repo_rules(&self.state, &req.project_id, audit_paths.as_ref().map(|paths| paths.as_slice())) {
            Ok(effective) => for note in effective.notes { input_binding.push_str(&format!("{note}\n")); },
            Err(error) => input_binding.push_str(&format!("INCOMPLETE policy coverage: {error}\n")),
        }
        if self.state.registry.get_meta(&req.project_id, quality_source::CORPUS_REVISION)
            .ok().flatten().is_some() {
            input_binding.push_str("Corpus reset: only sources imported after the last clear are searchable; other registry constraints may still be active.\n");
        }
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
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "{input_binding}Retrieval sampling: NOT RUN (empty corpus).\nPre-push audit: INACTIVE - 0 quality-gate rules are ingested; NOTHING was checked. Configure reviewed sources with ingest_quality_gates and re-run; until then this provider supplies no rule evidence."
            ))]));
        }
        let sampled = crate::utils::text::audit_code_query(&code);
        let sampling = input_binding + &sampled.coverage();
        if sampled.query.is_empty() {
            let count = match &count_failure {
                Some(failure) => format!("Rule count unavailable. FAILURE: {failure}"),
                None => format!("Known quality-gate rule count: {rules_total}."),
            };
            return Ok(CallToolResult::success(vec![Content::text(format!("{sampling}Retrieval NOT RUN: no usable identifier terms in bounded sample; no search run, no rules checked. {count}"))]));
        }
        let query = sampled.query;
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
                "{sampling}Pre-push audit: no quality-gate rules matched this change — {rules_total} rule(s) \
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
        out.push_str(&sampling);
        out.push_str(
            "Each is a rule/finding the team has flagged before (coding rules, copilot-instructions, \
             CodeRabbit/SonarQube history, the recurring-issues board). Confirm your change does NOT \
             violate them before pushing.\n\n",
        );
        out.push_str(&format!(
            "Applicability: read each full_rule before treating it as a mandate; complete source context may contain exceptions and examples.\n\nRetrieved (NOT verified against your code) — {} candidate rule(s) of {} in the namespace (top_k {}{})\n\n",
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
        let mut verifier = quality_source::SourceVerifier::new(
            &self.state, &req.project_id, std::path::Path::new(&ps.info.directory),
        );
        for h in &hits {
            let p = h.path.as_str().to_ascii_lowercase();
            let status = match ps.search.get_doc_by_doc_id(&req.project_id, QG_NAMESPACE, gen_, &h.doc_id) {
                Ok(Some((_, _, content, _, _))) => verifier.document_status(&h.doc_id, &content),
                _ => "unverified: full source evidence unavailable",
            };
            let coverage = if status.starts_with("current:") || status.starts_with("historical:") {
                "Evidence status"
            } else { "INCOMPLETE policy coverage; advisory evidence only" };
            let line = format!(
                "- [{}] Search excerpt (not the full rule): {}\n  {coverage}: {status}\n  full_rule: get_chunk({})",
                h.path.as_str(),
                h.snippet.as_deref().unwrap_or("").trim(),
                serde_json::json!({"project_id":req.project_id,"doc_id":h.doc_id,"namespace":QG_NAMESPACE})
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
        let origin = quality_source_origin(std::path::Path::new(&ps.info.directory), &abs)?;

        let findings = parse_quality_source_checked(&content, source, &origin)
            .map_err(|error| McpError::invalid_params(error, None))?;
        if matches!(source, QualitySource::CopilotInstructions | QualitySource::CodingRulesMd) {
            return Err(McpError::invalid_params(
                "Authored policies must retain their scope, exceptions and source context. Use ingest_quality_gates for copilot/rules sources; distill_quality_gates generalizes raw review findings. No rules imported.",
                None,
            ));
        }
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
             `{QG_NAMESPACE}` namespace [{sev}]. These are advisory suggestions; retrieval does not \
             establish their approval or applicability to a particular change.",
            rules.len(),
        ))]))
    }
}


#[cfg(test)]
mod audit_input_tests {
    use super::*;
    fn req(value:serde_json::Value)->crate::models::PrePushAuditRequest { serde_json::from_value(value).unwrap() }
    #[test]
    fn inline_compatibility_and_unambiguous_file_contract() {
        let root=tempfile::tempdir().unwrap();
        let inline=req(serde_json::json!({"project_id":"p","code":"ProposedCode"}));
        assert_eq!(resolve_audit_input(&inline,root.path()).unwrap(),"ProposedCode");
        for value in [serde_json::json!({"code":"", "diff_file":"a.patch"}),serde_json::json!({"code":"inline","diff_file":"a.patch","diff_file_blake3":"0".repeat(64)}),serde_json::json!({"diff_file_blake3":"0".repeat(64)})] {
            let mut value=value;value["project_id"]=serde_json::json!("p");
            assert!(resolve_audit_input(&req(value),root.path()).is_err());
        }
    }
    #[test]
    fn file_hash_path_and_encoding_verified_before_retrieval() {
        let root=tempfile::tempdir().unwrap();let data=b"diff --git a/a b/a\n@@ -1 +1 @@\n+TenantCheck\n";
        std::fs::write(root.path().join("a.patch"),data).unwrap();
        let hash=blake3::hash(data).to_hex().to_string();
        let request=|path:&str,hash:&str|req(serde_json::json!({"project_id":"p","diff_file":path,"diff_file_blake3":hash}));
        assert_eq!(resolve_audit_input(&request("a.patch",&hash),root.path()).unwrap().as_bytes(),data);
        for path in ["../a.patch","/a.patch","C:\\a.patch","C:a.patch","\\\\server\\share\\a.patch"] {
            assert!(resolve_audit_input(&request(path,&hash),root.path()).is_err());
        }
        assert!(resolve_audit_input(&request("a.patch",&"0".repeat(64)),root.path()).is_err());
        std::fs::write(root.path().join("bad.patch"),[255u8]).unwrap();
        assert!(resolve_audit_input(&request("bad.patch",&blake3::hash(&[255]).to_hex().to_string()),root.path()).is_err());
        std::fs::write(root.path().join("big.patch"),vec![b'x';4*1024*1024+1]).unwrap();
        assert!(resolve_audit_input(&request("big.patch",&hash),root.path()).is_err());
    }
}
