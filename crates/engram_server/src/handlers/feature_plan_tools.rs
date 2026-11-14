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
use std::{
    collections::{BTreeSet, VecDeque},
    io::Read,
    path::Path,
    time::Duration,
};

const MAX_SOURCE_FILES: usize = 24;
const MAX_COMPOSITION_HOSTS: usize = 4;

fn is_webforms_markup(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    path.ends_with(".aspx") || path.ends_with(".ascx")
}

fn initial_source_paths(paths: &[String]) -> Vec<String> {
    let mut selected: Vec<_> = paths.iter().take(plan::MAX_FILES).cloned().collect();
    // A ranked prefix can contain only libraries, schemas and translations.
    // Reserve source slots for WebForms hosts so their editor components can
    // be discovered without changing the underlying change-set ranking.
    for path in paths
        .iter()
        .filter(|path| is_webforms_markup(path))
        .take(MAX_COMPOSITION_HOSTS)
    {
        if !selected.contains(path) {
            selected.push(path.clone());
        }
    }
    selected
}

fn registered_components(
    directory: &Path,
    host: &SourceEvidence,
    normalized_host_hash: &str,
) -> super::page_composition::Composition {
    let page = directory.join(&host.file_path);
    let app = super::access_layer_tools::discover_web_application_root(directory, &page);
    let mut composition = super::page_composition::collect(directory, &app, &page, false, true);
    // Discovery reads the host again. A changed host cannot supply new leads
    // under its previously admitted content fingerprint. Composition uses
    // ContentHash's normalized line endings; SourceEvidence uses raw bytes.
    // Compare the same hash convention here, retaining raw-byte admission and
    // final freshness checks for every host and component.
    if !composition
        .files
        .iter()
        .any(|f| f.path == host.file_path && f.source_hash == normalized_host_hash)
    {
        composition.files.clear();
        composition
            .warnings
            .push("Host changed during composition discovery; component leads withheld".into());
    }
    composition
}

#[cfg(test)]
mod feature_plan_composition_tests {
    use super::*;

    #[test]
    fn webforms_hosts_survive_a_non_ui_candidate_prefix_without_reordering_it() {
        let mut paths: Vec<_> = (0..20)
            .map(|i| format!("Resources/translation{i}.resx"))
            .collect();
        paths.extend(
            [
                "Admin/Import.aspx",
                "Admin/Settings.aspx",
                "Other/Example.aspx",
                "Other/Fourth.aspx",
                "Other/Fifth.aspx",
            ]
            .map(String::from),
        );
        let selected = initial_source_paths(&paths);
        assert_eq!(&selected[..12], &paths[..12]);
        assert!(selected.contains(&"Admin/Settings.aspx".to_string()));
        assert_eq!(selected.len(), 16);
        assert!(!selected.contains(&"Other/Fifth.aspx".to_string()));
    }

    #[test]
    fn registered_editor_and_codebehind_are_found_but_changed_host_is_withheld() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::write(root.join("web.config"), "<configuration />").unwrap();
        std::fs::create_dir(root.join("widgets")).unwrap();
        let markup = "<%@ Page Language='VB' %>\r\n<%@ Register Src='~/widgets/Threshold.ascx' TagName='Threshold' TagPrefix='u' %>\r\n<u:Threshold runat='server' />\r\n";
        std::fs::write(root.join("Settings.aspx"), markup).unwrap();
        std::fs::write(root.join("widgets/Threshold.ascx"), "<%@ Control Language='VB' CodeFile='Threshold.ascx.vb' %><asp:TextBox ID='RequiredCount' runat='server' />").unwrap();
        std::fs::write(
            root.join("widgets/Threshold.ascx.vb"),
            "Public Class Threshold\nEnd Class\n",
        )
        .unwrap();
        let host = SourceEvidence {
            id: "E1".into(),
            file_path: "Settings.aspx".into(),
            source_hash: blake3::hash(markup.as_bytes()).to_hex().to_string(),
            start_line: 1,
            end_line: 1,
            excerpt: markup.into(),
            methods: vec![],
        };
        let normalized_hash = engram_core::ContentHash::compute(markup.as_bytes()).0;
        assert_ne!(normalized_hash, host.source_hash);
        let found = registered_components(root, &host, &normalized_hash);
        assert!(
            found
                .files
                .iter()
                .any(|f| f.path == "widgets/Threshold.ascx" && f.kind == "user_control")
        );
        assert!(
            found
                .files
                .iter()
                .any(|f| f.path == "widgets/Threshold.ascx.vb" && f.kind == "codebehind")
        );
        std::fs::write(root.join("Settings.aspx"), format!("{markup}\nchanged")).unwrap();
        let stale = registered_components(root, &host, &normalized_hash);
        assert!(stale.files.is_empty());
        assert!(!stale.warnings.is_empty());
    }
}

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
        max_tokens: None,
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
    let terms: BTreeSet<_> = concepts
        .iter()
        .flat_map(|s| super::planning_tools::concept_stems(s))
        .collect();
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

fn changed_source_evidence(directory: &str, evidence: &[SourceEvidence]) -> BTreeSet<String> {
    evidence.iter().filter_map(|row| {
        let current = (|| -> anyhow::Result<bool> {
            let mut bytes = Vec::new();
            engram_core::safe_open_read(Path::new(directory), &row.file_path)?
                .take(plan::MAX_SOURCE_BYTES as u64 + 1)
                .read_to_end(&mut bytes)?;
            Ok(bytes.len() <= plan::MAX_SOURCE_BYTES
                && blake3::hash(&bytes).to_hex().as_str() == row.source_hash)
        })().unwrap_or(false);
        (!current).then(|| row.file_path.clone())
    }).collect()
}

#[cfg(test)]
mod feature_plan_source_freshness_tests {
    use super::*;

    #[test]
    fn feature_plan_dossier_detects_a_changed_captured_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("Photos.aspx.vb");
        let source = "Public Class Photos\nEnd Class\n";
        std::fs::write(&path, source).unwrap();
        let evidence = vec![SourceEvidence {
            id: "E1".into(), file_path: "Photos.aspx.vb".into(),
            source_hash: blake3::hash(source.as_bytes()).to_hex().to_string(),
            start_line: 1, end_line: 2, excerpt: source.into(), methods: vec![],
        }];
        assert!(changed_source_evidence(temp.path().to_str().unwrap(), &evidence).is_empty());
        std::fs::write(&path, "Public Class ChangedPhotos\nEnd Class\n").unwrap();
        assert_eq!(changed_source_evidence(temp.path().to_str().unwrap(), &evidence),
            BTreeSet::from(["Photos.aspx.vb".to_owned()]));
    }
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
            "provenance": {"model": identity, "prompt_version":"feature-plan-v7", "provider_calls":0,
                "provider_calls_semantics":"Generation requests; transport retries follow the configured provider policy"},
            "limits": [
                "Model-generated requirements, edits, DDL and templates are proposals; source anchors do not verify their meaning.",
                "No project file edits, database execution, runtime verification or approval is performed.",
                "Candidate and pattern discovery may be incomplete; all nested coverage and recovery instructions remain applicable.",
                "Method safety is heuristic, not proof of safe edits. Files without touched existing methods have no method safety verdict.",
                "Source reads check selected revision and content hash but are not an atomic filesystem snapshot."
            ]
        });
        if identity.is_none() && !req.dossier_only {
            output["reason"] = json!("llm_backend_unavailable");
            return response(output);
        }
        let intent = if req.dossier_only {
            output["request"] = json!(&req);
            output["provenance"]["prompt_version"] = Value::Null;
            None
        } else {
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
                Err(error) => {
                    output["reason"] = json!("invalid_intent_contract");
                    output["rejection"] = plan::rejection_details(&error, &intent_text);
                    return response(output);
                }
            };
            Some(intent)
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
        let mut story = req.feature_description.clone();
        if let Some(area) = req.area_hint.as_deref().filter(|s| !s.trim().is_empty()) {
            story.push_str(&format!("\nArea hint: {area}"));
        }
        if let Some(model) = req.model_after.as_deref().filter(|s| !s.trim().is_empty()) {
            story.push_str(&format!("\nModel after: {model}"));
        }
        // Reuse the established story-domain extractor. Model-proposed role
        // names and prose phrases are intent, not a replacement search plan.
        let retrieval_concepts = super::planning_tools::extract_story_concepts(
            &super::planning_tools::story_for_concepts(&req.feature_description),
        );
        output["retrieval_concepts"] = json!(retrieval_concepts);
        let change_req = change_request(&req.project_id, &story, Some(retrieval_concepts.clone()));
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
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| retrieval_concepts.join(" "));
        let patterns = match self
            .implementation_pattern_evidence(FindImplementationPatternRequest {
                project_id: req.project_id.clone(),
                pattern_query,
                max_examples: 3,
                output_json: true,
                max_tokens:None, response_id:None, response_offset:0,
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
        let source_paths = initial_source_paths(&paths);
        let graph = self.state.graph.clone();
        let project_id = req.project_id.clone();
        let directory = record.directory.clone();
        let concepts = retrieval_concepts.clone();
        let worker_view = view.clone();
        let (evidence, gaps, composition) = tokio::task::spawn_blocking(move || {
            // A cancelled request must not release selected source ownership
            // while this non-cancellable worker is still reading it.
            let _source_hold = worker_view;
            let mut evidence = Vec::new();
            let mut gaps = Vec::new();
            let mut composition = Vec::new();
            let inventory_paths: Vec<_> = source_paths.iter()
                .filter(|path| selected.contains_key(path.as_str())).cloned().collect();
            let mut method_inventories = super::file_functions::collect(
                &graph, &project_id, &inventory_paths, 2000, super::file_functions::LimitCounts::Functions,
            ).ok();
            let mut queued: BTreeSet<_> = source_paths.iter().cloned().collect();
            let mut queue = VecDeque::from(source_paths);
            let mut bytes = 0;
            while let Some(path) = queue.pop_front() {
                let admitted = (|| -> anyhow::Result<(SourceEvidence, String)> {
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
                    // Registered editors arrive after the initial inventory.
                    // Gather the pending component group together, preserving
                    // source admission and queue order for every file below.
                    if let Some(files) = method_inventories.as_mut() {
                        if !files.contains_key(&path) {
                            let pending: Vec<_> = std::iter::once(&path).chain(queue.iter())
                                .filter(|p| selected.contains_key(p.as_str()) && !files.contains_key(p.as_str()))
                                .cloned().collect();
                            if let Ok(found) = super::file_functions::collect(
                                &graph, &project_id, &pending, 2000, super::file_functions::LimitCounts::Functions,
                            ) {
                                files.extend(found);
                            }
                        }
                    }
                    let nodes = match method_inventories.as_mut().and_then(|files| files.remove(&path)) {
                        Some(nodes) => nodes,
                        None => graph.query_nodes(&project_id, Some("function"), None, Some(&path), 2000)?,
                    };
                    let methods = nodes
                        .into_iter().filter(|n| n.file_path.as_str().replace('\\', "/") == path && n.generation == *revision && n.start_line >= start && n.start_line <= end)
                        .map(|n| MethodTarget {method_name:n.name.rsplit('.').next().unwrap_or(&n.name).to_owned(), line:n.start_line}).collect();
                    bytes += excerpt.len();
                    let normalized_hash = engram_core::ContentHash::compute(source.as_bytes()).0;
                    Ok((SourceEvidence {id:format!("E{}", evidence.len()+1), file_path:path.clone(), source_hash:hash, start_line:start, end_line:end, excerpt, methods}, normalized_hash))
                })();
                match admitted {
                    Ok((row, normalized_hash)) => {
                        if is_webforms_markup(&path) {
                            if composition.len() < MAX_COMPOSITION_HOSTS {
                                let mut found = registered_components(Path::new(&directory), &row, &normalized_hash);
                                let mut omitted = Vec::new();
                                for component in &found.files {
                                    if plan::relative_file(&component.path) && !queued.contains(&component.path) {
                                        if queued.len() < MAX_SOURCE_FILES {
                                            queued.insert(component.path.clone());
                                            queue.push_back(component.path.clone());
                                        } else {
                                            omitted.push(component.path.clone());
                                        }
                                    }
                                }
                                if !omitted.is_empty() {
                                    found.warnings.push("Component evidence file cap reached; omitted paths require separate reads".into());
                                }
                                composition.push(json!({"host":path,"discovery":found,"omitted_paths":omitted,
                                    "interpretation":"Static component leads, not source admission or runtime binding proof"}));
                            } else {
                                gaps.push(json!({"file_path":path,"reason":"Composition host cap reached; registered components not inspected"}));
                            }
                        }
                        evidence.push(row);
                    },
                    Err(_) => gaps.push(json!({"file_path":path, "reason":"Source unavailable, unselected, stale, non-UTF8 or exceeds evidence limits; no source claims admitted", "recovery":{"tool":"read_file","arguments":{"project_id":project_id,"file_path":path}}})),
                }
            }
            (evidence, gaps, composition)
        }).await.map_err(|_| failure("Feature source worker failed"))?;
        let omitted_paths: Vec<_> = paths
            .iter()
            .filter(|path| !evidence.iter().any(|row| &row.file_path == *path))
            .cloned()
            .collect();
        output["source_evidence"] = json!(evidence);
        output["source_coverage"] = json!({"file_cap":MAX_SOURCE_FILES, "primary_file_cap":plan::MAX_FILES, "composition_host_cap":MAX_COMPOSITION_HOSTS,
            "composition":composition, "candidate_files":paths.len(), "admitted_files":evidence.len(), "omitted_paths":omitted_paths, "gaps":gaps,
            "excerpt_limit":"One lexical window of at most 100 lines / 12 KiB per file; at most 192 KiB total; unshown lines are unexamined",
            "recovery":{"tool":"get_change_set","arguments":{"project_id":req.project_id,"story":story,"output_json":true,"detail":"full","concepts":retrieval_concepts}}});
        let mut schema = Vec::new();
        for name in intent.iter().flat_map(|intent| &intent.schema_names) {
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
        if req.dossier_only {
            let stale = changed_source_evidence(&record.directory, &evidence);
            if !stale.is_empty() || self.get_active_generation(&req.project_id).await? != generation {
                output["reason"] = json!("source_changed_during_planning");
                output["changed_files"] = json!(stale);
                return response(output);
            }
            output["status"] = json!("SOURCE_DOSSIER_READY");
            output["source_freshness"] = json!("CURRENT_AT_RETURN");
            output["planning_context"]["schema_status"] = json!("NOT_INFERRED_IN_DOSSIER_MODE");
            output["edit_safety_status"] = json!("NOT_ASSESSED_NO_PROPOSED_EDITS");
            return response(output);
        }
        let intent = intent.expect("generation mode parsed its intent before source intake");
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
            Err(error) => {
                let rejection = plan::rejection_details(&error, &plan_text);
                let Some(repair_prompt) =
                    plan::repair_prompt(&prompt, &plan_text, &rejection["validation"])
                else {
                    output["reason"] = json!("invalid_plan_contract_or_source_reference");
                    output["rejection"] = rejection;
                    return response(output);
                };
                output["repair"] = json!({"status":"ATTEMPTED", "maximum_attempts":1,
                    "initial_rejection":rejection});
                output["provenance"]["provider_calls"] = json!(3);
                let repaired_text = match self
                    .state
                    .dreaming
                    .generate_text(&repair_prompt, 12_288, Duration::from_secs(180))
                    .await
                {
                    Ok(text) => text,
                    Err(error) => {
                        output["repair"]["status"] = json!("PROVIDER_FAILED");
                        output["failure"] = json!(AnalysisDegradation::from_error(&error));
                        output["reason"] = json!("plan_repair_provider_failed");
                        return response(output);
                    }
                };
                match plan::parse_plan(&repaired_text, &intent, &evidence) {
                    Ok(plan) => {
                        output["repair"]["status"] =
                            json!("CONTRACT_VALIDATED_SEMANTICS_UNVERIFIED");
                        plan
                    }
                    Err(error) => {
                        output["repair"]["status"] = json!("REJECTED");
                        output["reason"] = json!("invalid_plan_contract_or_source_reference");
                        output["rejection"] = plan::rejection_details(&error, &repaired_text);
                        return response(output);
                    }
                }
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
        let stale = changed_source_evidence(&record.directory, &evidence);
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
