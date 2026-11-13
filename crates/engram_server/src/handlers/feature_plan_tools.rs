use crate::{
    models::*,
    services::{business_logic_service::AnalysisDegradation, feature_plan as plan},
    tools::Engram,
};
use plan::{MethodTarget, SourceEvidence};
use rmcp::{
    ErrorData as McpError,
    model::{CallToolResult, Content},
};
use serde_json::{Value, json};
use std::{collections::BTreeSet, io::Read, path::Path, time::Duration};

fn failure(message: impl Into<String>) -> McpError {
    McpError::internal_error(message.into(), None)
}

fn body(result: CallToolResult) -> Result<Value, McpError> {
    if result.is_error == Some(true) {
        return Err(failure("Planning evidence tool returned an error"));
    }
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .ok_or_else(|| failure("Planning evidence tool returned no text"))?;
    serde_json::from_str(&text.text)
        .map_err(|_| failure("Planning evidence was not structured JSON"))
}

fn response(value: Value) -> Result<CallToolResult, McpError> {
    let text = serde_json::to_string(&value).map_err(|_| failure("Cannot render feature plan"))?;
    // All evidence and model bounds are accounted for. Never silently cut a plan.
    if text.len() > 1024 * 1024 {
        return Err(failure(
            "Feature plan exceeds 1 MiB response bound; narrow the feature or area",
        ));
    }
    Ok(CallToolResult::success(vec![Content::text(text)]))
}

fn change_request(
    project: &str,
    story: &str,
    concepts: Option<Vec<String>>,
) -> GetChangeSetRequest {
    GetChangeSetRequest {
        project_id: project.into(),
        story: story.into(),
        concepts,
        response_id: None,
        response_offset: None,
        merged_before: None,
        as_of_rev: None,
        include_ui_contract: Some(true),
        work_item_text: None,
        pat_token: None,
        output_json: true,
        detail: Some("compact".into()),
        expand_concepts: false,
    }
}

fn path_candidates(change_set: &Value, patterns: &Value) -> Vec<String> {
    let mut paths = Vec::new();
    // Keep ranked candidates first; dependencies and patterns remain explicit
    // context even when not admitted to the bounded source bundle.
    for key in ["files", "asset_dependencies", "caller_dependencies"] {
        for row in change_set[key].as_array().into_iter().flatten() {
            if let Some(path) = row["path"].as_str() {
                let path = path.replace('\\', "/");
                if plan::relative_file(&path) && !paths.contains(&path) {
                    paths.push(path);
                }
            }
        }
    }
    for row in patterns["exemplars"].as_array().into_iter().flatten() {
        if let Some(path) = row["path"].as_str() {
            let path = path.replace('\\', "/");
            if plan::relative_file(&path) && !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    paths
}

/// Whole, numbered lines around the strongest lexical window. Every unshown
/// line is explicitly outside the plan's source evidence; no whole-file claim.
fn excerpt(source: &str, concepts: &[String]) -> (u32, u32, String) {
    let lines: Vec<_> = source.lines().collect();
    if lines.is_empty() {
        return (0, 0, String::new());
    }
    let terms: Vec<_> = concepts.iter().map(|s| s.to_lowercase()).collect();
    let scores: Vec<usize> = lines
        .iter()
        .map(|line| {
            let lower = line.to_lowercase();
            terms
                .iter()
                .filter(|term| lower.contains(term.as_str()))
                .count()
        })
        .collect();
    let center = (0..lines.len())
        .max_by_key(|&i| {
            (
                scores[i.saturating_sub(12)..(i + 13).min(lines.len())]
                    .iter()
                    .sum::<usize>(),
                std::cmp::Reverse(i),
            )
        })
        .unwrap_or(0);
    let start = center.saturating_sub(35);
    let mut out = String::new();
    let mut end = start;
    for (index, line) in lines.iter().enumerate().skip(start).take(100) {
        let numbered = format!("{}: {}\n", index + 1, line);
        if out.len() + numbered.len() > 12 * 1024 {
            break;
        }
        out.push_str(&numbered);
        end = index + 1;
    }
    (start as u32 + 1, end as u32, out)
}

impl Engram {
    pub async fn handle_one_shot_feature_plan(
        &self,
        req: OneShotFeaturePlanRequest,
    ) -> Result<CallToolResult, McpError> {
        super::validate_project_id(&req.project_id)?;
        plan::validate_request(&req).map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        let record = self.ensure_project_record(&req.project_id).await?;
        let identity = self.state.dreaming.text_generation_identity()
            .map(|(provider, model)| json!({"provider": provider, "requested_model": model, "resolved_model": null}));
        let mut output = json!({
            "status": "INCOMPLETE", "project_id": req.project_id,
            "detail_level": req.detail_level, "plan": null,
            "provenance": {"model": identity, "prompt_version":"feature-plan-v1", "provider_calls":0},
            "limits": [
                "Model-generated requirements, edits, DDL and templates are proposals; source anchors do not verify their meaning.",
                "No project file edits, database execution, runtime verification or approval is performed.",
                "Candidate and pattern discovery may be incomplete; all nested coverage and recovery instructions remain applicable.",
                "Method safety is heuristic, not proof of safe edits. Files without touched existing methods have no method safety verdict.",
                "Source reads check selected revision and content hash but are not an atomic filesystem snapshot."
            ]
        });
        if identity.is_none() {
            output["reason"] = json!("llm_backend_unavailable");
            return response(output);
        }
        output["provenance"]["provider_calls"] = json!(1);
        let intent_text = match self
            .state
            .dreaming
            .generate_text(&plan::intent_prompt(&req), 3072, Duration::from_secs(90))
            .await
        {
            Ok(text) => text,
            Err(e) => {
                output["failure"] = json!(AnalysisDegradation::from_error(&e));
                output["reason"] = json!("intent_provider_failed");
                return response(output);
            }
        };
        let intent = match plan::parse_intent(&intent_text) {
            Ok(intent) => intent,
            Err(_) => {
                output["reason"] = json!("invalid_intent_contract");
                return response(output);
            }
        };
        output["intent"] = json!(intent);
        let runtime = self.ensure_project_runtime(&req.project_id).await?;
        let (generation, view) = self
            .pin_active_source_generation(&req.project_id, &runtime.search)
            .await?;
        let selected = view
            .selected_file_revisions()
            .ok_or_else(|| {
                failure(
                    "Feature planning requires a managed source inventory; update_project first",
                )
            })?
            .clone();
        let view = std::sync::Arc::new(view);
        output["active_generation"] = json!(generation);
        let story = format!(
            "{}\nArea hint: {}\nModel after: {}",
            req.feature_description,
            req.area_hint.as_deref().unwrap_or("unspecified"),
            req.model_after.as_deref().unwrap_or("unspecified")
        );
        let change_req = change_request(&req.project_id, &story, Some(intent.concepts.clone()));
        let mut change_set = body(self.handle_get_change_set(change_req.clone()).await?)?;
        // Reassemble an existing immutable response, never rerun ranking.
        if let Some(id) = change_set["response_delivery"]["response_id"]
            .as_str()
            .map(str::to_owned)
        {
            let total = change_set["response_delivery"]["original_bytes"]
                .as_u64()
                .unwrap_or(u64::MAX);
            if total > 512 * 1024 {
                output["reason"] = json!("change_set_exceeds_planning_context");
                output["change_set"] = change_set;
                return response(output);
            }
            let mut text = String::new();
            loop {
                let mut recovery = change_request(&req.project_id, "", None);
                recovery.response_id = Some(id.clone());
                recovery.response_offset = Some(text.len());
                let page = body(self.handle_get_change_set(recovery).await?)?;
                let part = page["content"]
                    .as_str()
                    .ok_or_else(|| failure("Invalid change-set recovery"))?;
                if part.is_empty() || text.len() + part.len() > total as usize {
                    return Err(failure("Non-advancing or oversized change-set recovery"));
                }
                text.push_str(part);
                if page["complete"] == true {
                    break;
                }
            }
            if text.len() != total as usize || blake3::hash(text.as_bytes()).to_hex().as_str() != id
            {
                return Err(failure("Change-set recovery identity mismatch"));
            }
            change_set = serde_json::from_str(&text)
                .map_err(|_| failure("Recovered change set is not JSON"))?;
        }
        if !change_set["files"].is_array() {
            output["reason"] = json!("incomplete_change_set_intake");
            output["change_set"] = change_set;
            return response(output);
        }
        let pattern_query = req
            .model_after
            .as_deref()
            .unwrap_or(&req.feature_description)
            .to_owned();
        let patterns = match self
            .handle_find_implementation_pattern(FindImplementationPatternRequest {
                project_id: req.project_id.clone(),
                pattern_query,
                max_examples: 3,
                output_json: true,
            })
            .await
            .and_then(body)
        {
            Ok(value) => value,
            Err(_) => {
                json!({"status":"UNAVAILABLE", "recovery":{"tool":"find_implementation_pattern","arguments":{"project_id":req.project_id,"pattern_query":req.feature_description,"output_json":true}}})
            }
        };
        let paths = path_candidates(&change_set, &patterns);
        let source_paths = paths
            .iter()
            .take(plan::MAX_FILES)
            .cloned()
            .collect::<Vec<_>>();
        let omitted_paths = paths
            .iter()
            .skip(plan::MAX_FILES)
            .cloned()
            .collect::<Vec<_>>();
        let graph = self.state.graph.clone();
        let project_id = req.project_id.clone();
        let directory = record.directory.clone();
        let concepts = intent.concepts.clone();
        let worker_view = view.clone();
        let (evidence, gaps) = tokio::task::spawn_blocking(move || {
            // A cancelled request must not release selected source ownership
            // while this non-cancellable worker is still reading it.
            let _source_hold = worker_view;
            let mut evidence = Vec::new();
            let mut gaps = Vec::new();
            let mut bytes = 0;
            for path in source_paths {
                let admitted = (|| -> anyhow::Result<SourceEvidence> {
                    let revision = selected.get(path.as_str()).ok_or_else(|| anyhow::anyhow!("not_selected"))?;
                    let mut source = String::new();
                    engram_core::safe_open_read(Path::new(&directory), &path)?.take(plan::MAX_SOURCE_BYTES as u64 + 1).read_to_string(&mut source)?;
                    anyhow::ensure!(source.len() <= plan::MAX_SOURCE_BYTES, "source_limit");
                    let file = graph.get_node(&project_id, &format!("file:{path}"))?.ok_or_else(|| anyhow::anyhow!("missing_file_fingerprint"))?;
                    anyhow::ensure!(file.generation == *revision, "revision_mismatch");
                    let hash = blake3::hash(source.as_bytes()).to_hex().to_string();
                    anyhow::ensure!(file.metadata.as_ref().and_then(|m| m.get("file_hash")).and_then(Value::as_str) == Some(hash.as_str()), "stale_source");
                    let (start, end, excerpt) = excerpt(&source, &concepts);
                    anyhow::ensure!(!excerpt.is_empty() && start <= end, "no_bounded_source_excerpt");
                    anyhow::ensure!(bytes + excerpt.len() <= plan::MAX_EVIDENCE_BYTES, "aggregate_evidence_limit");
                    let methods = graph.query_nodes(&project_id, Some("function"), None, Some(&path), 2000)?
                        .into_iter().filter(|n| n.file_path.as_str().replace('\\', "/") == path && n.generation == *revision && n.start_line >= start && n.start_line <= end)
                        .map(|n| MethodTarget {method_name:n.name.rsplit('.').next().unwrap_or(&n.name).to_owned(), line:n.start_line}).collect();
                    bytes += excerpt.len();
                    Ok(SourceEvidence {id:format!("E{}", evidence.len()+1), file_path:path.clone(), source_hash:hash, start_line:start, end_line:end, excerpt, methods})
                })();
                match admitted {
                    Ok(row) => evidence.push(row),
                    Err(_) => gaps.push(json!({"file_path":path, "reason":"Source unavailable, unselected, stale, non-UTF8 or exceeds evidence limits; no source claims admitted", "recovery":{"tool":"read_file","arguments":{"project_id":project_id,"file_path":path}}})),
                }
            }
            (evidence, gaps)
        }).await.map_err(|_| failure("Feature source worker failed"))?;
        output["source_evidence"] = json!(evidence);
        output["source_coverage"] = json!({"file_cap":plan::MAX_FILES, "candidate_files":paths.len(), "admitted_files":evidence.len(), "omitted_paths":omitted_paths, "gaps":gaps, "excerpt_limit":"One lexical window of at most 100 lines / 12 KiB per file; unshown lines are unexamined", "recovery":{"tool":"get_change_set","arguments":{"project_id":req.project_id,"story":story,"output_json":true,"detail":"full","concepts":intent.concepts}}});
        let mut schema = Vec::new();
        for name in &intent.schema_names {
            let result = self
                .handle_query_schema(QuerySchemaRequest {
                    project_id: req.project_id.clone(),
                    query_type: SchemaQueryType::Table { name: name.clone() },
                    offset: None,
                    limit: Some(20),
                    max_tokens: Some(4096),
                })
                .await
                .and_then(body);
            schema.push(match result {
                Ok(value) => json!({"name":name,"result":value}),
                Err(_) => json!({"name":name,"status":"UNAVAILABLE","recovery":{"tool":"query_schema","arguments":{"project_id":req.project_id,"query_type":{"kind":"table","name":name}}}}),
            });
        }
        let context = json!({"change_set":change_set, "patterns":patterns, "schema":schema, "source_coverage":output["source_coverage"]});
        output["planning_context"] = context.clone();
        if evidence.is_empty() {
            output["reason"] = json!("no_current_source_evidence");
            return response(output);
        }
        if context.to_string().len() > 192 * 1024 {
            output["reason"] = json!("planning_context_limit");
            return response(output);
        }
        output["provenance"]["provider_calls"] = json!(2);
        let prompt = plan::assembly_prompt(&req, &intent, &evidence, &context);
        let plan_text = match self
            .state
            .dreaming
            .generate_text(&prompt, 12_288, Duration::from_secs(180))
            .await
        {
            Ok(text) => text,
            Err(e) => {
                output["failure"] = json!(AnalysisDegradation::from_error(&e));
                output["reason"] = json!("plan_provider_failed");
                return response(output);
            }
        };
        let mut proposal = match plan::parse_plan(&plan_text, &intent, &evidence) {
            Ok(plan) => plan,
            Err(_) => {
                output["reason"] = json!("invalid_plan_contract_or_source_reference");
                return response(output);
            }
        };
        // A proposed new path must not overwrite an unindexed or hidden file.
        for new_file in &proposal.new_files {
            let path = engram_core::safe_join(Path::new(&record.directory), &new_file.file_path)
                .map_err(|_| failure("Unsafe proposed new-file path"))?;
            if path.try_exists().unwrap_or(true) {
                output["reason"] = json!("proposed_new_file_already_exists_or_unverifiable");
                return response(output);
            }
        }
        let mut safety = Vec::new();
        for file in &proposal.modified_files {
            if file.methods.is_empty() {
                safety.push(json!({"file_path":file.file_path,"status":"NO_EXISTING_METHOD_VERDICT","reason":"Markup/schema/new-method edit or existing methods unspecified; no method-level safety claim"}));
            }
            for target in &file.methods {
                let result = self
                    .handle_check_edit_safety(CheckEditSafetyRequest {
                        project_id: req.project_id.clone(),
                        file_path: file.file_path.clone(),
                        method_name: target.method_name.clone(),
                        line: Some(target.line),
                        class_name: None,
                        output_json: true,
                        downstream_detail: ImpactDetail::Summary,
                    })
                    .await
                    .and_then(body);
                safety.push(match result {
                    Ok(result) => json!({"file_path":file.file_path, "method":target, "status":"ASSESSED","result":result}),
                    Err(_) => json!({"file_path":file.file_path, "method":target, "status":"UNAVAILABLE","recovery":{"tool":"check_edit_safety","arguments":{"project_id":req.project_id,"file_path":file.file_path,"method_name":target.method_name,"line":target.line,"output_json":true}}}),
                });
            }
        }
        output["edit_safety"] = json!(safety);
        // Do not return a current proposal after files changed during provider calls.
        let mut stale = BTreeSet::new();
        for row in &evidence {
            let current = (|| -> anyhow::Result<bool> {
                let mut bytes = Vec::new();
                engram_core::safe_open_read(Path::new(&record.directory), &row.file_path)?
                    .take(plan::MAX_SOURCE_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)?;
                Ok(bytes.len() <= plan::MAX_SOURCE_BYTES
                    && blake3::hash(&bytes).to_hex().as_str() == row.source_hash)
            })()
            .unwrap_or(false);
            if !current {
                stale.insert(&row.file_path);
            }
        }
        if !stale.is_empty() || self.get_active_generation(&req.project_id).await? != generation {
            output["reason"] = json!("source_changed_during_planning");
            output["changed_files"] = json!(stale);
            return response(output);
        }
        plan::project_detail(&mut proposal, req.detail_level);
        output["status"] = json!("DRAFT_REQUIRES_SOURCE_AND_REQUIREMENT_REVIEW");
        output["structural_validation"] = json!({"references_and_order":"PASS","semantic_correctness":"UNVERIFIED","runtime_behavior":"UNVERIFIED"});
        output["plan"] = json!(proposal);
        response(output)
    }
}
