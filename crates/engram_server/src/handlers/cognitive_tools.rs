use crate::handlers::validate_project_id;
use crate::models::{
    AntiPatternGuardRequest, AstDependencyGraphRequest, ComputeBlastRadiusRequest,
    DetectDesignPatternsRequest, DreamProjectRequest, ExportCapturePackRequest,
    GetExtractionConfidenceRequest, GetUiBlueprintRequest, GraphCentralityRerankRequest,
    ImmuneCheckRequest, ImpactAnalysisRequest, MapAjaxRegionsRequest, ProjectIdRequest,
    QueryBusinessLogicRequest, TraceStateUsageRequest, TraceUiActionRequest, TraceUiEventRequest,
};
use crate::services::{cognitive_service, job_service};
use crate::state::AppEvent;
use crate::tools::Engram;
use crate::utils::now_ms;
use engram_core::safe_join;
use engram_graph::EdgeKind;
use engram_graph::store::ResolveResult;
use engram_index::HybridQuery;
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use std::fmt::Write;
use std::path::PathBuf;

/// (node_id, node_type, name, file_path) tuple used in centrality reranking.
type NodeMetaTuple = (String, Option<String>, Option<String>, Option<String>);

fn codemap_followup(mut action: serde_json::Value, path_contains: Option<&str>) -> serde_json::Value {
    if let Some(term) = path_contains {
        action["arguments"]["path_contains"] = serde_json::json!(term);
    }
    action
}

fn named_source_fallback(row: &mut serde_json::Value, project: &str) {
    let has_read=row["symbols"].as_array().is_some_and(|symbols|symbols.iter().any(|symbol|
        symbol["actions"].as_array().is_some_and(|actions|actions.iter().any(|a|a["tool"]=="get_full_method_body"))));
    if !has_read && !row["actions"].as_array().unwrap().iter().any(|a|a["tool"]=="get_full_method_body") {
        let path=row["path"].clone();
        row["actions"].as_array_mut().unwrap().push(serde_json::json!({"tool":"get_full_method_body",
            "arguments":{"project_id":project,"file_path":path,"context_lines":0,"output_json":true}}));
    }
}

/// Supplement a selected-file orientation with one current snapshot. Indexed
/// navigation remains untouched; only trailing complete preview lines may go.
fn codemap_source_preview(
    mut navigation: serde_json::Value, directory: &std::path::Path, project: &str,
    targeted: bool, source_contains: Option<&str>, header: &str, footer: &str,
) -> Result<String, String> {
    const REPLY_BYTES: usize = 12 * 1024;
    const PREVIEW_BYTES: usize = 8 * 1024;
    const LINES: u64 = 120;
    let render = |value: &serde_json::Value| -> Result<String, String> {
        let encoded = serde_json::to_string(value).map_err(|e| e.to_string())?;
        Ok(format!("{header}\n## CodeMap Navigation\n\n```json\n{encoded}\n```\n{footer}"))
    };
    if render(&navigation)?.len() > REPLY_BYTES {
        return Err("CodeMap metadata exceeds the12KiB source-preview profile; narrow to a symbol_name/symbol_node_id or retry with include_source=false. No navigation facts were discarded.".into());
    }
    let files = navigation["files"].as_array().ok_or("Missing selected file navigation")?;
    if files.len() != 1 { return Err("include_source requires one exact selected file".into()); }
    let row = &files[0];
    let file = row["path"].as_str().ok_or("Missing selected file path")?.to_string();
    let symbols = row["symbols"].as_array();
    // A source-page anchor is inspection context, never overload resolution.
    // Ambiguous, display-clipped and unfinished inventories use the file start.
    let anchor = symbols.filter(|nodes| targeted && nodes.len() == 1
        && row["declarations_observed"] == 1 && row["symbol_display_limited"] == false)
        .and_then(|nodes| nodes.first()).filter(|node| node["start_line"].as_u64().is_some_and(|n| n > 0));
    let start = anchor.and_then(|node| node["start_line"].as_u64()).map_or(1, |line| (line - 1) / LINES * LINES + 1);
    let anchor_id = anchor.and_then(|node| node["node_id"].as_str()).map(str::to_string);
    let recovery = |line: u64| serde_json::json!({"tool":"get_full_method_body","arguments":{
        "project_id":project,"file_path":file,"line_start":line,"context_lines":0,
        "include_caller_bodies":false,"output_json":true}});
    let unavailable = |reason: String, line: u64| serde_json::json!({"status":"unavailable",
        "file_path":file,"reason":reason,"recovery":recovery(line),
        "scope":"Indexed navigation preserved; current source not supplied. No freshness or binding proof."});
    let snapshot = super::access_layer_tools::read_current_source_snapshot(directory, &file);
    let (source, hash) = match snapshot {
        Ok(snapshot) => snapshot,
        Err(error) => {
            navigation["current_source"] = unavailable(error.chars().take(240).collect(), 1);
            let out = render(&navigation)?;
            return if out.len() <= REPLY_BYTES { Ok(out) } else { Err("Source-preview unavailability metadata exceeds12KiB; narrow the symbol selection or use include_source=false".into()) };
        }
    };
    if let Some(literal) = source_contains {
        return codemap_literal_inspection(navigation, &file, &source, &hash, project, literal, header, footer);
    }
    let total = source.lines().count() as u64;
    if (total == 0 && start != 1) || (total > 0 && start > total) {
        navigation["current_source"] = unavailable("Indexed anchor page lies beyond current file; inspect from the beginning. Indexed method bounds are unverified.".into(), 1);
        let out = render(&navigation)?;
        return if out.len() <= REPLY_BYTES { Ok(out) } else { Err("Source-preview metadata exceeds12KiB; narrow the selection".into()) };
    }
    let mut lines = Vec::new();
    let mut raw_bytes = 0usize;
    for line in source.lines().skip((start - 1) as usize).take(LINES as usize) {
        let next = raw_bytes.saturating_add(line.len()).saturating_add(usize::from(!lines.is_empty()));
        if next > PREVIEW_BYTES { break; }
        lines.push(line); raw_bytes = next;
    }
    loop {
        if lines.is_empty() && total > 0 {
            navigation["current_source"] = unavailable("No complete source line fits the preview budget; no line was clipped or skipped.".into(), start);
            let out = render(&navigation)?;
            return if out.len() <= REPLY_BYTES { Ok(out) } else { Err("Source-preview metadata exceeds12KiB; narrow the selection".into()) };
        }
        let end = if total == 0 { 0 } else { start + lines.len() as u64 - 1 };
        navigation["current_source"] = serde_json::json!({"status":"available","file_path":file,
            "line_start":if total==0{0}else{start},"line_end":end,"total_lines":total,
            "source_code":lines.join("\n"),"source_file_hash":hash,"source_file_hash_algorithm":"blake3",
            "source_verification":"current_file_snapshot_not_index_verified","indexed_anchor_node_id":anchor_id,
            "scope":"Current excerpt, not verified indexed method bounds, binding or project freshness.",
            "next_page":if end<total{Some(recovery(end+1))}else{None}});
        let preview_bytes = serde_json::to_vec(&navigation["current_source"]).map_err(|e|e.to_string())?.len();
        let out = render(&navigation)?;
        if preview_bytes <= PREVIEW_BYTES && out.len() <= REPLY_BYTES { return Ok(out); }
        if lines.pop().is_none() {
            return Err("Source-preview metadata exceeds12KiB; narrow the selection or use include_source=false".into());
        }
    }
}

/// Current literal occurrences are inspection leads, never graph declarations
/// or calls. The selected-file navigation and its coverage remain unchanged.
fn codemap_literal_inspection(
    mut navigation: serde_json::Value, file: &str, source: &str, hash: &str,
    project: &str, literal: &str, header: &str, footer: &str,
) -> Result<String, String> {
    const MATCHES: usize = 8;
    const CONTEXT: u64 = 12;
    let render = |value: &serde_json::Value| -> Result<String, String> {
        Ok(format!("{header}\n## CodeMap Navigation\n\n```json\n{}\n```\n{footer}",
            serde_json::to_string(value).map_err(|e|e.to_string())?))
    };
    let read = |start: u64, end: Option<u64>| {
        let mut arguments=serde_json::json!({"project_id":project,"file_path":file,"line_start":start,
            "context_lines":0,"include_caller_bodies":false,"output_json":true});
        if let Some(end)=end {arguments["line_end"]=serde_json::json!(end);}
        serde_json::json!({"tool":"get_full_method_body","arguments":arguments})
    };
    let total=source.lines().count() as u64;
    let mut hits=Vec::new();let mut lookahead=None;let mut scanned=0;
    for (index,line) in source.lines().enumerate() {
        scanned=index as u64+1;
        if line.as_bytes().windows(literal.len()).any(|part|part.eq_ignore_ascii_case(literal.as_bytes())) {
            if hits.len()==MATCHES {lookahead=Some(scanned);break;}
            hits.push(scanned);
        }
    }
    let mut groups: Vec<(u64,u64,Vec<u64>)>=Vec::new();
    for &line in &hits {
        let start=line.saturating_sub(CONTEXT).max(1);let end=line.saturating_add(CONTEXT).min(total);
        if let Some(previous)=groups.last_mut().filter(|previous|end-previous.0<120) {
            previous.1=end;previous.2.push(line);
        }else{groups.push((start,end,vec![line]));}
    }
    // Reserve the largest possible bounded recovery before accepting source.
    // A later omission must not invalidate previously accepted windows. The
    // reservation is only byte accounting; the returned action stays literal.
    let recovery_budget=if groups.is_empty(){0}else{
        serde_json::to_vec(&read(total,Some(total))).map_err(|e|e.to_string())?.len()
    };
    let fits = |value: &serde_json::Value| -> Result<bool, String> {
        let actual=serde_json::to_vec(&value["current_source"]["recovery"]).map_err(|e|e.to_string())?.len();
        let reserved=recovery_budget.saturating_sub(actual);
        Ok(serde_json::to_vec(&value["current_source"]).map_err(|e|e.to_string())?.len()+reserved<=8*1024
            && render(value)?.len()+reserved<=12*1024)
    };
    let mut preview=serde_json::json!({"status":if hits.is_empty(){"not_observed"}else{"available"},
        "mode":"literal_inspection","file_path":file,"total_lines":total,
        "source_file_hash":hash,"source_file_hash_algorithm":"blake3",
        "source_verification":"current_file_snapshot_not_index_verified",
        "source_contains":literal,"match_case":"ASCII_insensitive_Unicode_exact",
        "matched_lines":hits,"matching_line_limit":MATCHES,
        "matching_lines_observed":hits.len()+usize::from(lookahead.is_some()),
        "scan_complete":lookahead.is_none(),"scanned_through_line":scanned,
        "lookahead_line":lookahead,"more_matches_unassessed":lookahead.is_some(),
        "windows_observed":groups.len(),"windows":[],"windows_omitted":groups.len(),
        "sampled_matching_lines_source_omitted":hits.len(),
        "recovery":lookahead.map(|line|read(line,None)),
        "scope":"Current literal text and nearby lines, not declarations, calls, value flow, binding or project freshness. Context may contain unsampled matches; recovery reads fresh source, not a continued search."});
    navigation["current_source"]=preview.clone();
    if !fits(&navigation)? {return Err("Literal-inspection metadata exceeds the source-preview budget; use a narrower scope or a separate source read".into());}
    let mut rows=Vec::new();let mut inline_matches=0;let mut inline_windows=0;let mut first_missing=None;
    for (start,end,matches) in &groups {
        let mut row=serde_json::json!({"line_start":start,"line_end":end,"matched_lines":matches,
            "source_read":read(*start,Some(*end))});
        let mut code=String::new();let mut raw_fits=true;let mut line_count=0;
        for line in source.lines().skip((*start-1) as usize).take((end-start+1) as usize) {
            if code.len().saturating_add(line.len()).saturating_add(usize::from(line_count>0))>8*1024 {
                raw_fits=false;break;
            }
            if line_count>0{code.push('\n');}code.push_str(line);line_count+=1;
        }
        let may_inline=raw_fits && inline_windows<3;
        if may_inline {row["source_code"]=serde_json::json!(code);}
        else {row["source_omitted"]=serde_json::json!(true);}
        rows.push(row);
        preview["windows"]=serde_json::json!(&rows);
        preview["windows_omitted"]=serde_json::json!(groups.len()-rows.len());
        preview["sampled_matching_lines_source_omitted"]=serde_json::json!(hits.len()-inline_matches);
        navigation["current_source"]=preview.clone();
        let supplied=may_inline && fits(&navigation)?;
        if !supplied {
            first_missing.get_or_insert((*start,*end));
            let (missing_start,missing_end)=first_missing.unwrap();
            preview["recovery"]=read(missing_start,Some(missing_end));
            let last=rows.last_mut().unwrap().as_object_mut().unwrap();
            last.remove("source_code");last.insert("source_omitted".into(),serde_json::json!(true));
            preview["windows"]=serde_json::json!(&rows);navigation["current_source"]=preview.clone();
            if !fits(&navigation)? {rows.pop();}
        }else{inline_matches+=matches.len();inline_windows+=1;}
    }
    preview["windows"]=serde_json::json!(&rows);
    preview["windows_omitted"]=serde_json::json!(groups.len()-rows.len());
    preview["sampled_matching_lines_source_omitted"]=serde_json::json!(hits.len()-inline_matches);
    if let Some((start,end))=first_missing {preview["recovery"]=read(start,Some(end));}
    navigation["current_source"]=preview;
    if !fits(&navigation)? {return Err("Literal-inspection recovery metadata exceeds12KiB; use a separate bounded source read".into());}
    render(&navigation)
}

/// A directory/name lookup has its own compact profile. File pagination
/// counts attempted identities, never the number of matching declarations.
fn directory_named_navigation(
    req: &crate::models::CodebaseOverviewRequest,
    paths: &engram_index::hybrid::SelectedSourceNavigation,
    nodes: Vec<engram_graph::Node>, mut coverage: serde_json::Value,
    generation: u64, managed: bool,
) -> anyhow::Result<serde_json::Value> {
    let page=|offset,limit|codemap_followup(serde_json::json!({"tool":"get_codebase_overview","arguments":{
        "project_id":req.project_id,"scope":req.scope,"symbol_name":req.symbol_name,
        "exclude_paths":req.exclude_paths,"offset":offset,"limit":limit,"include_declarations":true}}),req.path_contains.as_deref());
    let attempted=coverage["files_attempted"].as_u64().unwrap_or(0) as usize;
    let next=if attempted<paths.files.len() {Some(req.offset+attempted)}else{paths.next_offset};
    let retry=coverage["partial_file"].as_str().or_else(||
        (attempted==0).then(||paths.files.first().map(|file|file.0.as_str())).flatten());
    let retry=retry.map(|path|codemap_followup(serde_json::json!({"tool":"get_codebase_overview","arguments":{
        "project_id":req.project_id,"scope":path,"symbol_name":req.symbol_name,
        "exclude_paths":req.exclude_paths,"limit":1}}),req.path_contains.as_deref()));
    coverage["managed_source"]=serde_json::json!(managed);
    coverage["matching_files"]=serde_json::json!(paths.matching_files);
    coverage["file_scan_truncated"]=serde_json::json!(paths.scan_truncated);
    coverage["display_omitted_candidates"]=serde_json::json!(0);
    let mut files=Vec::new();
    for (path,revision,_) in &paths.files {
        let mut found:Vec<_>=nodes.iter().filter(|node|node.file_path.as_str()==path).collect();
        found.sort_by(|a,b|a.node_id.cmp(&b.node_id));
        if found.is_empty() {continue;}
        let symbols:Vec<_>=found.into_iter().map(|node|{
            let mut source=serde_json::json!({"project_id":req.project_id,"file_path":path,
                "context_lines":0,"output_json":true});
            if node.start_line>0 && node.end_line>=node.start_line {
                source["line_start"]=serde_json::json!(node.start_line);
                source["line_end"]=serde_json::json!(node.end_line.min(node.start_line.saturating_add(119)));
            }
            serde_json::json!({"node_id":node.node_id,"name":node.name,"kind":node.node_type,
                "start_line":node.start_line,"end_line":node.end_line,"generation":node.generation,
                "actions":[{"tool":"get_full_method_body","arguments":source},
                    {"tool":"impact_analysis","arguments":{"project_id":req.project_id,"symbol_fqn":node.node_id,
                        "max_depth":6,"limit":12,"detail":"summary"}}]})
        }).collect();
        files.push(serde_json::json!({"path":path,"physical_generation":revision,"symbols":symbols}));
    }
    let mut out=serde_json::json!({"format":"engram-codemap-navigation-v1","source_generation":generation,
        "scope":req.scope,"symbol_name":req.symbol_name,"exclude_paths":req.exclude_paths,
        "working_tree_checked":false,"detail":"directory_exact_name_candidates","coverage":coverage,
        "files":files,"next_page":next.filter(|offset|*offset>req.offset).map(|offset|page(offset,req.limit)),
        "retry_file":retry,"display_retry":null,"omitted_candidates":[],
        "limitations":[
            "Exact native callable name under one held selected file page; candidates are not uniqueness, inventory, runtime binding or absence proof. Other kinds, legacy identities and aliases remain unassessed.",
            "Shared file/prefix/decode/candidate limits; deadlines checked between synchronous operations, which can overrun. Unfinished work/display omissions stay explicit; follow-ups are fresh, not atomic.",
            "Source actions check current bytes, not method bounds or freshness (indexed ranges capped120lines). For method context expand exact-file scope+symbol_node_id. No current source read here."
        ]});
    if let Some(term)=req.path_contains.as_deref(){out["path_contains"]=serde_json::json!(term);}
    if attempted==0 && !paths.files.is_empty() {
        out["retry_page"]=page(req.offset,req.limit);
        out["retry_page_scope"]=serde_json::json!("Fresh retry of this selected-file page after zero attempts; not a progress cursor");
    }
    // Keep every retained identity even if its full actions do not fit.
    // Retrying the directory page would loop for crowded single-file names;
    // the literal retry expands one omitted identity independently instead.
    while serde_json::to_vec(&out)?.len()>56*1024 {
        let files=out["files"].as_array_mut().unwrap();
        let file=files.last_mut().ok_or_else(||anyhow::anyhow!("Directory name metadata exceeds response bound; narrow exclusions/scope"))?;
        let path=file["path"].as_str().unwrap().to_owned();
        let removed=file["symbols"].as_array_mut().unwrap().pop().expect("nonempty candidate file");
        let id=removed["node_id"].as_str().unwrap();
        if file["symbols"].as_array().unwrap().is_empty(){files.pop();}
        let omitted=out["omitted_candidates"].as_array_mut().unwrap();
        if let Some(group)=omitted.iter_mut().find(|group|group["path"]==path) {
            group["node_ids"].as_array_mut().unwrap().push(serde_json::json!(id));
        }else{omitted.push(serde_json::json!({"path":path,"node_ids":[id]}));}
        out["display_retry"]=codemap_followup(serde_json::json!({"tool":"get_codebase_overview","arguments":{
            "project_id":req.project_id,"scope":path,"symbol_node_id":id,"exclude_paths":req.exclude_paths,"limit":1}}),req.path_contains.as_deref());
        out["display_retry_scope"]=serde_json::json!("One omitted identity; other retained IDs are listed, not an offset continuation");
        let omitted=out["coverage"]["display_omitted_candidates"].as_u64().unwrap()+1;
        out["coverage"]["display_omitted_candidates"]=serde_json::json!(omitted);
    }
    Ok(out)
}

/// Observe a known identity under the already-held file selection. This is
/// deliberately separate from bounded declaration discovery and name lookup.
fn exact_navigation_node(
    graph: &engram_graph::GraphStore,
    membership: &engram_index::hybrid::SelectedSourceRevisionLookup,
    project: &str,
    path: &str,
    id: &str,
) -> (Vec<engram_graph::Node>, serde_json::Value) {
    use engram_graph::store::{ImpactAdmission, ImpactWalkCoverage};
    let mut coverage = ImpactWalkCoverage::default();
    let mut stage = "target";
    let mut records = 0;
    let mut bytes = 0;
    let attempted = (|| -> anyhow::Result<Option<engram_graph::Node>> {
        records += 1;
        let Some((node, charged)) = graph.get_node_bounded(project, id, 64 * 1024)? else {
            coverage.missing_nodes += 1;
            coverage.reasons.push("exact_symbol_target_missing".into());
            return Ok(None);
        };
        bytes += charged;
        if node.file_path.as_str() != path || node.node_type == "file" {
            coverage.unknown_nodes += 1;
            coverage.reasons.push("exact_symbol_wrong_file_or_kind".into());
            return Ok(None);
        }
        if node.name.len() > 2048 || node.node_type.len() > 64 {
            coverage.unknown_nodes += 1;
            coverage.reasons.push("exact_symbol_identity_fields_exceed_limit".into());
            return Ok(None);
        }
        stage = "file";
        records += 1;
        let Some((file, charged)) = graph.get_node_bounded(project, &format!("file:{path}"), 64 * 1024)? else {
            coverage.missing_nodes += 1;
            coverage.reasons.push("exact_symbol_file_missing".into());
            return Ok(None);
        };
        bytes += charged;
        stage = "admission";
        match crate::services::impact_walk_service::admit_selected_source(membership, &node, Some(&file))? {
            ImpactAdmission::Selected => Ok(Some(node)),
            ImpactAdmission::NonSelectedRevision => {
                coverage.non_selected_nodes += 1;
                coverage.reasons.push("exact_symbol_non_selected_revision".into());
                Ok(None)
            }
            _ => {
                coverage.unknown_nodes += 1;
                coverage.reasons.push("exact_symbol_source_admission_unknown".into());
                Ok(None)
            }
        }
    })();
    let nodes: Vec<_> = match attempted {
        Ok(node) => node.into_iter().collect(),
        Err(_) => {
            coverage.unknown_nodes += 1;
            coverage.reasons.push(format!("exact_symbol_{stage}_unavailable"));
            Vec::new()
        }
    };
    coverage.nodes_observed = nodes.len();
    coverage.truncated = nodes.is_empty();
    let observation = serde_json::json!({"observation":"exact_bounded_identity_lookup",
        "declaration_inventory":"not_scanned","record_limit_bytes":64*1024,
        "records_attempted":records,"raw_record_bytes_decoded":bytes,
        "declaration_admission":coverage});
    (nodes, observation)
}

/// Score a trace path from 0.0 (unusable) to 1.0 (high-certainty direct
/// evidence). Starts from 1.0 and subtracts a penalty per hop based on
/// the hop's edge kind (inferred from the `(prev, curr)` node-type pair),
/// then subtracts any caller-supplied `fallback_penalty` (from ambiguous
/// control-lookup resolution). Result is floored at 0.10 so a path that
/// exists but is wholly declarative still reports something non-zero.
///
/// Rationale: declarative data-binding hops (control → control via
/// `DataSourceID`) fire on ASP.NET's schedule, not from a user action,
/// so they carry genuinely less certainty than direct event-wiring hops
/// (control → function via `OnClick` / `Handles`). Users reading the
/// provenance should see paths ranked accordingly.
fn path_confidence_score(path: &[engram_graph::Node], fallback_penalty: f64) -> f64 {
    let mut score = 1.0_f64;
    for w in path.windows(2) {
        let prev = w[0].node_type.as_str();
        let curr = w[1].node_type.as_str();
        let hop_penalty = match (prev, curr) {
            ("control", "control") => 0.25,   // DataBinding — declarative
            ("page", "control") => 0.05,      // Contains — structural
            ("page", "class") => 0.05,        // Inherits
            ("control", "function") => 0.00,  // EventWiring — direct
            ("function", "function") => 0.05, // Calls — indirect but typed
            (_, "inline_sql") | (_, "stored_proc") => 0.00, // terminal evidence
            _ => 0.10,                        // unknown / generic dependency
        };
        score -= hop_penalty;
    }
    score -= fallback_penalty;
    score.clamp(0.10, 1.0)
}

/// True iff `file_pattern` (from a `RepoRule`) matches `target_path`. The
/// supported forms mirror what `inject_repo_rules` understands: exact
/// equality, globs with `*` / `?`, and plain substring for short patterns
/// without metacharacters. Slash direction is normalised so Windows
/// callers don't get tripped up by backslashes.
/// What an anti-pattern hit prints. The indexed document is
/// `ANTI-PATTERN / Original Commit / Reverted in Commit / Path` + blank
/// line + the reverted code: the metadata header is always shown (it is
/// the evidence an agent needs), the CODE only with `include_content`
/// (row-8 audit A6 — before, everything printed unconditionally).
fn antipattern_hit_text(snippet: Option<&str>, include_content: bool) -> String {
    let Some(s) = snippet else {
        return "(no snippet)".into();
    };
    if include_content {
        return s.to_string();
    }
    let header: Vec<&str> = s.lines().take_while(|l| !l.trim().is_empty()).collect();
    let mut out = header.join("\n");
    out.push_str("\n(code hidden — pass include_content=true to see the reverted code)");
    out
}

// Missing evidence must not erase a positive violation or imply clearance.
fn immune_status(verdict_rank: u8, incomplete_evidence: bool) -> &'static str {
    match verdict_rank {
        2 => "🔴 BLOCKED",
        1 => "🟡 WARNING",
        _ if incomplete_evidence => "INSUFFICIENT",
        _ => "🟢 CLEAN",
    }
}

fn immune_rule_matches_path(file_pattern: &str, target_path: &str) -> bool {
    if file_pattern.is_empty() {
        return false;
    }
    let pat = file_pattern.replace('\\', "/").to_lowercase();
    let path = target_path.replace('\\', "/").to_lowercase();
    if pat == path {
        return true;
    }
    if pat.contains('*') || pat.contains('?') {
        // Minimal glob → regex: escape everything except `*` and `?`.
        let mut re = String::with_capacity(pat.len() + 8);
        re.push('^');
        for c in pat.chars() {
            match c {
                '*' => re.push_str(".*"),
                '?' => re.push('.'),
                '.' | '+' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^' | '$' | '\\' => {
                    re.push('\\');
                    re.push(c);
                }
                _ => re.push(c),
            }
        }
        re.push('$');
        if let Ok(compiled) = regex::Regex::new(&re) {
            return compiled.is_match(&path);
        }
        return false;
    }
    // Fall back to substring for bare non-glob patterns.
    path.contains(&pat)
}

/// Scan a snippet for deterministically-dangerous operations. Returns a
/// sorted, de-duplicated list of pattern names that fired — used for both
/// verdict escalation and human-readable output.
///
/// The list is intentionally conservative: every matcher here is a
/// pattern that is rarely benign on a DAL / DDL surface. False-positive
/// tolerance is low because firing this list only proposes a verdict
/// floor — the full ladder still consults similarity and match counts.
fn detect_destructive_patterns(code: &str) -> Vec<String> {
    use std::sync::LazyLock;
    // (name, pattern) — names flow through to the output, patterns are
    // compiled once.
    static PATTERNS: LazyLock<Vec<(&'static str, regex::Regex)>> = LazyLock::new(|| {
        let raw: &[(&str, &str)] = &[
            // LINQ-to-SQL bulk helpers.
            ("DeleteAllOnSubmit", r"(?i)\bDeleteAllOnSubmit\s*\("),
            ("InsertAllOnSubmit", r"(?i)\bInsertAllOnSubmit\s*\("),
            // EF Core bulk helpers.
            ("RemoveRange", r"(?i)\bRemoveRange\s*\("),
            ("ExecuteDelete", r"(?i)\bExecuteDelete\s*\("),
            // Raw SQL DDL and bulk mutations. `\b` on both sides keeps
            // these from firing on `TRUNCATED_COLUMN_NAME` identifiers.
            ("DROP TABLE", r"(?i)\bDROP\s+TABLE\b"),
            ("TRUNCATE TABLE", r"(?i)\bTRUNCATE\s+TABLE\b"),
            // DELETE FROM — any occurrence is suspicious on a DAL file.
            // Deliberately broad: a false positive only proposes an
            // escalation floor, it never blocks on its own. Refining
            // this into "DELETE without WHERE" needs a real SQL parser
            // and is not worth the complexity at this stage.
            ("DELETE FROM", r"(?i)\bDELETE\s+FROM\s+[\[\]\w.]+"),
            // Raw SQL passed through ADO.NET / Dapper / EF execution.
            // When `ExecuteNonQuery` / `ExecuteSql` / `Execute` appears
            // ANYWHERE in the same snippet as a `DELETE` / `DROP` /
            // `TRUNCATE` literal, that's the textbook shape we flag.
            (
                "ExecuteNonQuery + destructive SQL",
                r#"(?is)\bExecute(?:NonQuery|Sql|SqlRaw|SqlInterpolated)\b[\s\S]*?\b(?:DELETE|DROP|TRUNCATE)\b|\b(?:DELETE|DROP|TRUNCATE)\b[\s\S]*?\bExecute(?:NonQuery|Sql|SqlRaw|SqlInterpolated)\b"#,
            ),
        ];
        raw.iter()
            .filter_map(|(name, pat)| regex::Regex::new(pat).ok().map(|re| (*name, re)))
            .collect()
    });
    let mut hits: Vec<String> = PATTERNS
        .iter()
        .filter(|(_, re)| re.is_match(code))
        .map(|(name, _)| name.to_string())
        .collect();
    hits.sort();
    hits.dedup();
    hits
}

fn format_ambiguous_symbol_error(input: &str, candidates: &[engram_graph::Node]) -> String {
    let details = candidates
        .iter()
        .take(10)
        .map(|n| {
            format!(
                "  - {} [{}] {}",
                n.node_id,
                n.node_type,
                n.file_path.as_str()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Symbol '{input}' is ambiguous ({} matches):\n{}\n\nPass a fully-qualified node_id (e.g., sym:/file:/table:) to disambiguate.",
        candidates.len(),
        details
    )
}

impl Engram {
    pub(crate) async fn cancel_job_internal(
        &self,
        job_id: &str,
    ) -> job_service::CancellationOutcome {
        job_service::cancel_job_internal(&self.state, job_id).await
    }

    pub(crate) async fn cognitive_analyze_file_style(
        &self,
        project_id: &str,
        file_path: &str,
        diff_limit: usize,
    ) -> cognitive_service::StyleAnalysisResult {
        cognitive_service::analyze_file_style(&self.state, project_id, file_path, diff_limit).await
    }

    pub(crate) async fn cognitive_suggest_boundaries(
        &self,
        project_id: &str,
        min_frequency: u32,
        max_clusters: usize,
        timeout_secs: u64,
        include_cross_cluster_deps: bool,
    ) -> anyhow::Result<Vec<engram_ml::MigrationBoundary>> {
        cognitive_service::suggest_migration_boundaries(
            &self.state,
            project_id,
            min_frequency,
            max_clusters,
            timeout_secs,
            include_cross_cluster_deps,
        )
        .await
    }

    pub async fn handle_impact_analysis(
        &self,
        req: ImpactAnalysisRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        if !(1..=8).contains(&req.max_depth) {
            return Err(McpError::invalid_params("max_depth must be 1..8", None));
        }
        if req.file_path.is_some() == req.symbol_fqn.is_some() {
            return Err(McpError::invalid_params("Provide exactly one of file_path or symbol_fqn", None));
        }
        let (mut result, _source_hold, generation) = crate::services::impact_walk_service::collect_with_detail(
            &self.state, &req.project_id, req.file_path.clone(), req.symbol_fqn.clone(),
            req.max_depth, req.limit, req.detail,
        ).await;
        if req.detail==crate::models::ImpactDetail::Summary {
            if let Some(generation)=generation {
                result.push_str(&self.freshness_footer(&req.project_id,generation).await);
            }
            if result.len()>crate::services::impact_walk_service::SUMMARY_BYTES {
                return Err(McpError::internal_error("Impact summary exceeds response budget",None));
            }
            return Ok(CallToolResult::success(vec![Content::text(result)]));
        }
        result.push_str(&self.historical_impact_observations(
            &req.project_id, req.file_path.clone(), req.symbol_fqn.clone(),
            _source_hold.clone(), generation,
        ).await);
        if let Some(ref path) = req.file_path {
            result.push_str(&self.confidence_footer(
                &engram_core::RelPath::from(path.as_str()),
                engram_core::guess_language(std::path::Path::new(path)),
            ));
        }
        if let Some(generation) = generation {
            result.push_str(&self.freshness_footer(&req.project_id, generation).await);
        } else {
            result.push_str("\nSource generation unavailable; unavailable evidence is not zero impact.\n");
        }
        Ok(CallToolResult::success(vec![Content::text(result)]))
    }

    /// Historical co-change is separate live-graph evidence, never a causal
    /// witness or a claim that the observed companion is current source.
    async fn historical_impact_observations(
        &self,
        project: &str,
        file: Option<String>,
        symbol: Option<String>,
        source: Option<std::sync::Arc<engram_index::SourceSnapshotGuard>>,
        generation: Option<u64>,
    ) -> String {
        let graph=self.state.graph.clone();
        let project=project.to_owned();
        let runtime=match self.ensure_project_runtime(&project).await {
            Ok(runtime)=>runtime,
            Err(error)=>return format!("\nHistorical companion observations unavailable: {error}.\n"),
        };
        let observed=tokio::task::spawn_blocking(move || -> anyhow::Result<serde_json::Value> {
            let _source_hold=source;
            let target=match (file.as_deref(),symbol.as_deref()) {
                (Some(path),_)=>format!("file:{path}"),
                (_,Some(symbol))=>{
                    if graph.get_node(&project,symbol)?.is_some(){symbol.to_owned()}
                    else{match graph.resolve_symbol(&project,symbol,None,None)?{
                        ResolveResult::Unique(node)=>node.node_id,
                        _=>anyhow::bail!("Historical target unavailable or ambiguous"),
                    }}
                },
                _=>anyhow::bail!("Historical target missing"),
            };
            anyhow::ensure!(target.len()<=2048 && !target.chars().any(char::is_control),
                "Historical target exceeds bounded identity metadata");
            let mut endpoints=vec![target.clone()];
            let mut component_limited=false;
            let mut component_coverage=None;
            if let Some(path)=file.as_deref() {
                if let (Some(view),Some(generation))=(_source_hold.as_ref(),generation) {
                    let membership=runtime.search.selected_source_revision_lookup(&project,generation,view.clone())?;
                    let admit=|node:&engram_graph::Node,file:Option<&engram_graph::Node>|
                        crate::services::impact_walk_service::admit_selected_source(&membership,node,file);
                    let (members,coverage)=graph.bounded_file_navigation_seeds(&project,&[path.to_owned()],&admit)?;
                    component_limited=coverage.truncated || members.len()>15;
                    component_coverage=Some(coverage);
                    endpoints.extend(members.into_iter().filter(|id|id!=&target).take(15));
                }else{component_limited=true;}
            }
            let component=endpoints.iter().cloned().collect();
            let adjacency=graph.component_adjacency(&project,&endpoints,&component,
                &|kind|if *kind==engram_graph::EdgeKind::TemporalCoupling{32}else{0},&|_|0)?;
            let mut companions=std::collections::BTreeMap::<String,u32>::new();
            for (source,_,weight,_) in adjacency.incoming {
                anyhow::ensure!(source.len()<=2048 && !source.chars().any(char::is_control),
                    "Historical companion exceeds bounded identity metadata");
                companions.entry(source).and_modify(|old|*old=(*old).max(weight)).or_insert(weight);
            }
            let count=companions.len();
            let mut examples:Vec<_>=companions.into_iter().collect();
            examples.sort_by(|a,b|b.1.cmp(&a.1).then_with(||a.0.cmp(&b.0)));
            let rows:Vec<_>=examples.into_iter().take(8).map(|(node_id,weight)|
                serde_json::json!({"node_id":node_id,"weight":weight})).collect();
            Ok(serde_json::json!({"historical_companions_observed":count,
                "count_is_lower_bound":component_limited || !adjacency.truncated_in.is_empty(),
                "examples_omitted":count-rows.len(),"examples":rows,"component_coverage":component_coverage,
                "scope":"Bounded live-graph temporal-coupling observations, separate from revision-admitted causal exposure. Companion endpoints and present applicability were not validated; co-change is not breakage or runtime proof.",
                "limits":{"component_endpoints":16,"accepted_external_per_endpoint":32,"examples":8},
                "recovery":{"tool":"find_symbol_references","arguments":{"project_id":project,"symbol_name":target,
                    "edge_kind_filter":["temporal_coupling"],"max_incoming":64,"max_outgoing_per_kind":8}},
                "recovery_scope":"Direct target co-change lookup; for capped file components explore declarations with scoped CodeMap and inspect their exact identities separately."}))
        }).await;
        match observed {
            Ok(Ok(value))=>format!("\n## Historical companion observations\n\n```json\n{}\n```\n",value),
            Ok(Err(error))=>format!("\nHistorical companion observations unavailable: {}. Unknown is not zero companions.\n",error.to_string().chars().take(512).collect::<String>()),
            Err(error)=>format!("\nHistorical companion observations worker unavailable: {}. Unknown is not zero companions.\n",error.to_string().chars().take(512).collect::<String>()),
        }
    }

    pub async fn handle_get_table_schema(
        &self,
        req: crate::models::GetTableSchemaRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let _ = self.ensure_project_runtime(&req.project_id).await?;

        let graph = self.state.graph.clone();
        let out = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let table_id = engram_core::ids::NodeId::table(&req.table_name).0;

            let table_node = graph
                .get_node(&req.project_id, &table_id)
                .map_err(|e| e.to_string())?;

            let Some(table_node) = table_node else {
                let candidates = graph
                    .query_nodes(
                        &req.project_id,
                        Some("db_table"),
                        Some(&req.table_name),
                        None,
                        10,
                    )
                    .map_err(|e| e.to_string())?;
                if candidates.is_empty() {
                    return Ok(format!(
                        "Table '{}' not found. Make sure the project has .sql DDL files indexed.",
                        req.table_name
                    ));
                }
                let names: Vec<_> = candidates.iter().map(|n| n.name.as_str()).collect();
                return Ok(format!(
                    "Table '{}' not found exactly. Did you mean one of: {}?",
                    req.table_name,
                    names.join(", ")
                ));
            };

            let mut out = format!("## Table: {}\n\n", table_node.name);
            out.push_str(&format!(
                "Source: {} (indexed DDL; verify against the deployed database)\n\n",
                table_node.file_path
            ));

            if let Some(ref meta) = table_node.metadata
                && let Some(ddl) = meta.get("ddl").and_then(|v| v.as_str())
            {
                out.push_str("### DDL\n```sql\n");
                out.push_str(ddl);
                out.push_str("\n```\n\n");
            }

            let mut columns = graph
                .neighbors(&req.project_id, EdgeKind::HasColumn, &table_id, 201)
                .map_err(|e| e.to_string())?;
            if columns.len() > 200 {
                out.push_str(
                    "INCOMPLETE: columns and foreign keys truncated to the first 200 columns.\n",
                );
                columns.truncate(200);
            }

            if !columns.is_empty() {
                out.push_str("### Columns\n");
                for (col_id, _weight) in &columns {
                    if let Some(col_node) = graph
                        .get_node(&req.project_id, col_id)
                        .map_err(|e| e.to_string())?
                    {
                        let data_type = col_node
                            .metadata
                            .as_ref()
                            .and_then(|m| m.get("data_type"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("?");
                        let nullable = col_node
                            .metadata
                            .as_ref()
                            .and_then(|m| m.get("nullable"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("?");
                        out.push_str(&format!(
                            "- **{}** {} (nullable: {})\n",
                            col_node.name, data_type, nullable
                        ));
                    } else {
                        out.push_str(&format!(
                            "- INCOMPLETE: column node unavailable: {col_id}\n"
                        ));
                    }
                }
                out.push('\n');
            }

            let mut fk_lines = Vec::new();
            for (col_id, _) in &columns {
                let mut fks = graph
                    .neighbors(&req.project_id, EdgeKind::ForeignKey, col_id, 51)
                    .map_err(|e| e.to_string())?;
                if fks.len() > 50 {
                    out.push_str(&format!(
                        "INCOMPLETE: foreign keys for {col_id} truncated at 50.\n"
                    ));
                    fks.truncate(50);
                }
                for (ref_col_id, _) in fks {
                    fk_lines.push(format!("- {} -> {}", col_id, ref_col_id));
                }
            }
            if !fk_lines.is_empty() {
                out.push_str("### Foreign Keys\n");
                for line in &fk_lines {
                    out.push_str(line);
                    out.push('\n');
                }
                out.push('\n');
            }

            let mut referencing = graph
                .find_incoming_edges(&req.project_id, Some(EdgeKind::QueriesTable), &table_id, 51)
                .map_err(|e| e.to_string())?;
            if referencing.len() > 50 {
                out.push_str("INCOMPLETE: SQL references truncated at 50.\n");
                referencing.truncate(50);
            }

            if !referencing.is_empty() {
                out.push_str("### Referenced by SQL Nodes\n");
                for (sql_id, weight) in &referencing {
                    let mut callers = graph
                        .find_incoming_edges(&req.project_id, Some(EdgeKind::SqlCalls), sql_id, 21)
                        .map_err(|e| e.to_string())?;
                    if callers.len() > 20 {
                        out.push_str(&format!(
                            "INCOMPLETE: callers for {sql_id} truncated at 20.\n"
                        ));
                        callers.truncate(20);
                    }
                    let caller_strs: Vec<_> = callers.iter().map(|(id, _)| id.as_str()).collect();
                    if caller_strs.is_empty() {
                        out.push_str(&format!("- {} (weight: {})\n", sql_id, weight));
                    } else {
                        out.push_str(&format!(
                            "- {} (weight: {}) <- called from: {}\n",
                            sql_id,
                            weight,
                            caller_strs.join(", ")
                        ));
                    }
                }
            }

            Ok(out)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_trace_state_usage(
        &self,
        req: TraceStateUsageRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        if req.state_type.trim().is_empty() || req.state_key.trim().is_empty() {
            return Err(McpError::invalid_params(
                "state_type and state_key must not be blank",
                None,
            ));
        }
        let _ = self.ensure_project_runtime(&req.project_id).await?;

        let project_id_outer = req.project_id.clone();
        let graph = self.state.graph.clone();
        let out = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let state_id = engram_core::ids::NodeId::state(&req.state_type, &req.state_key).0;

            let state_node = graph
                .get_node(&req.project_id, &state_id)
                .map_err(|e| format!("DB error looking up state node: {e}"))?;

            if state_node.is_none() {
                let candidates = graph
                    .query_nodes(
                        &req.project_id,
                        Some("global_state"),
                        Some(&req.state_key),
                        None,
                        20,
                    )
                    .map_err(|e| format!("DB error querying state candidates: {e}"))?;
                if candidates.is_empty() {
                    return Ok(format!(
                        "State key '{}[\"{}\"]' not found in the graph.\nMake sure the project has C#/VB files with {} access indexed.",
                        req.state_type, req.state_key, req.state_type
                    ));
                }
                let names: Vec<_> = candidates.iter().map(|n| n.name.as_str()).collect();
                return Ok(format!(
                    "State key '{}:{}' not found exactly. Similar keys found: {}",
                    req.state_type,
                    req.state_key,
                    names.join(", ")
                ));
            }

            let mut out = format!(
                "## State Usage: {}[\"{}\"]\n\n",
                req.state_type, req.state_key
            );

            out.push_str("Coverage: locations are indexed containing-symbol declarations, not verified access-site positions. Current source and runtime behavior are unverified.\n\n");
            // MCP1: use sanitized limit to prevent resource amplification.
            let limit = req.sanitized_limit();
            let writers = graph
                .find_incoming_edges(
                    &req.project_id,
                    Some(EdgeKind::WritesState),
                    &state_id,
                    limit + 1,
                )
                .map_err(|e| format!("DB error querying writers: {e}"))?;

            if writers.len() > limit { out.push_str(&format!("Coverage: writers truncated at {limit}.\n")); }
            if !writers.is_empty() {
                out.push_str("### Writers\n");
                for (writer_id, weight) in writers.iter().take(limit) {
                    if let Some(node) = graph.get_node(&req.project_id, writer_id).map_err(|e| format!("Writer node lookup failed: {e}"))? {
                        out.push_str(&format!(
                            "- {} [{}] in {}:{} (weight: {})\n",
                            node.name,
                            node.node_type,
                            node.file_path.as_str(),
                            node.start_line,
                            weight
                        ));
                    } else {
                        out.push_str(&format!("- {} (weight: {}; indexed node unavailable)\n", writer_id, weight));
                    }
                }
                out.push('\n');
            } else {
                out.push_str("### Writers\nNo indexed writers found; dynamic access may be absent from the graph.\n\n");
            }

            let readers = graph
                .find_incoming_edges(
                    &req.project_id,
                    Some(EdgeKind::ReadsState),
                    &state_id,
                    limit + 1,
                )
                .map_err(|e| format!("DB error querying readers: {e}"))?;

            if readers.len() > limit { out.push_str(&format!("Coverage: readers truncated at {limit}.\n")); }
            let mut property_readers = Vec::new();
            if !readers.is_empty() {
                out.push_str("### Readers\n");
                for (reader_id, weight) in readers.iter().take(limit) {
                    if let Some(node) = graph.get_node(&req.project_id, reader_id).map_err(|e| format!("Reader node lookup failed: {e}"))? {
                        out.push_str(&format!(
                            "- {} [{}] in {}:{} (weight: {})\n",
                            node.name,
                            node.node_type,
                            node.file_path.as_str(),
                            node.start_line,
                            weight
                        ));
                        if node.node_type == "property" {
                            property_readers.push(node);
                        }
                    } else {
                        out.push_str(&format!("- {} (weight: {}; indexed node unavailable)\n", reader_id, weight));
                    }
                }
            } else {
                out.push_str("### Readers\nNo indexed readers found; dynamic access may be absent from the graph.\n");
            }

            // The direct state edge identifies the property accessor. Follow one
            // typed Calls hop to expose the actual behavioral consumers as well.
            let mut indirect = std::collections::BTreeMap::new();
            let mut indirect_truncated = false;
            for accessor in &property_readers {
                let callers = graph
                    .find_incoming_edges(
                        &req.project_id,
                        Some(EdgeKind::Calls),
                        &accessor.node_id,
                        limit + 1,
                    )
                    .map_err(|e| format!("DB error querying property consumers: {e}"))?;
                if callers.len() > limit {
                    indirect_truncated = true;
                }
                for (caller_id, weight) in callers.into_iter().take(limit) {
                    indirect
                        .entry(caller_id)
                        .or_insert_with(|| (accessor.name.clone(), weight));
                }
            }
            if !indirect.is_empty() {
                out.push_str("\n### Indirect Readers Through Properties\n");
                out.push_str("Coverage: one static Calls hop from each property that directly reads this state; reflection and runtime dispatch remain unverified.\n");
                for (caller_id, (accessor_name, weight)) in indirect.iter().take(limit) {
                    if let Some(node) = graph.get_node(&req.project_id, caller_id).map_err(|e| format!("Property consumer lookup failed: {e}"))? {
                        out.push_str(&format!(
                            "- {} [{}] in {}:{} -> {} (weight: {})\n",
                            node.name,
                            node.node_type,
                            node.file_path.as_str(),
                            node.start_line,
                            accessor_name,
                            weight
                        ));
                    } else {
                        out.push_str(&format!(
                            "- {} -> {} (weight: {}; indexed node unavailable)\n",
                            caller_id, accessor_name, weight
                        ));
                    }
                }
                if indirect.len() > limit || indirect_truncated {
                    out.push_str(&format!("Coverage: indirect property readers truncated at {limit}.\n"));
                }
            }
            out.push_str(
                "\nnext: writers before readers when changing the shape of this value; \
                 detect_incomplete_changes(edited_files=[...]) after editing any \
                 subset of these sites — shared state is the classic missed-companion.\n",
            );

            Ok(out)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        let mut result = out;
        let gen_ = self.get_active_generation(&project_id_outer).await?;
        result.push_str(&self.freshness_footer(&project_id_outer, gen_).await);
        Ok(CallToolResult::success(vec![Content::text(result)]))
    }

    pub async fn handle_trace_ui_event(
        &self,
        req: TraceUiEventRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let _ = self.ensure_project_runtime(&req.project_id).await?;

        let mut start_id = if let Some(ref ctrl) = req.control_id {
            engram_core::ids::NodeId::control(&req.page_path, ctrl).0
        } else if let Some(ref handler) = req.handler_fqn {
            engram_core::ids::NodeId::symbol("function", Some(handler), &req.page_path, "", 0).0
        } else {
            engram_core::ids::NodeId::page(&req.page_path).0
        };

        let mut trace_used_fallback = false;
        let mut trace_candidate_count: usize = 0;
        let mut unresolved_candidates: Vec<String> = Vec::new();
        let start_missing = self
            .state
            .graph
            .get_node(&req.project_id, &start_id)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .is_none();
        if start_missing && let Some(ref ctrl) = req.control_id {
            match self
                .state
                .graph
                .resolve_symbol(&req.project_id, ctrl, Some("control"), Some(&req.page_path))
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
            {
                ResolveResult::Unique(node) => {
                    trace_used_fallback = true;
                    trace_candidate_count = 1;
                    unresolved_candidates = vec![node.node_id.clone()];
                    start_id = node.node_id;
                }
                ResolveResult::Ambiguous(candidates) => {
                    return Err(McpError::invalid_params(
                        format_ambiguous_symbol_error(ctrl, &candidates),
                        None,
                    ));
                }
                ResolveResult::NotFound => {}
            }
        } else if start_missing && let Some(ref handler) = req.handler_fqn {
            // Symbol node IDs are location-based; an FQN start point must be
            // resolved via resolve_symbol (exact name → metadata fqn →
            // terminal segment).
            match self
                .state
                .graph
                .resolve_symbol(&req.project_id, handler, Some("function"), None)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
            {
                ResolveResult::Unique(node) => {
                    start_id = node.node_id;
                }
                ResolveResult::Ambiguous(candidates) => {
                    return Err(McpError::invalid_params(
                        format_ambiguous_symbol_error(handler, &candidates),
                        None,
                    ));
                }
                ResolveResult::NotFound => {}
            }
        }

        let paths = self
            .state
            .graph
            .find_ui_paths(
                &req.project_id,
                &start_id,
                req.sanitized_max_hops(),
                req.sanitized_max_paths(),
            )
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if paths.is_empty() {
            // The MOST COMMON legacy case is a handler that never reaches
            // SQL (or wiring the extractor missed) — a bare dead-end here
            // wasted the agent's most likely follow-up moves.
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "No paths found from {start_id} to any SQL nodes within {} hops.\n\
                 next steps to trace it anyway:\n\
                 - raise max_hops (deep pages often need 4)\n\
                 - find_symbol_references(<the handler>) — the data access may live \
                 in a helper the handler calls\n\
                 - trace_ui_action(control_id=...) for the DOM/AJAX side of the wiring\n\
                 - if the control lives on a MasterPage/UserControl, trace from THAT \
                 file's handler instead\n\
                 - grep_project(\"<control id>\") to see every literal wiring site",
                req.sanitized_max_hops()
            ))]));
        }

        let mut out = format!("Found {} path(s) to SQL:\n", paths.len());

        let fallback_penalty = if trace_used_fallback {
            (trace_candidate_count as f64 * 0.2).min(0.8)
        } else {
            0.0
        };

        // Score every path by hop kinds. Paths that cross a
        // `control → control` hop (DataBinding — GridView DataSourceID
        // binding to a LinqDataSource / ObjectDataSource / SqlDataSource)
        // are declarative: the handler fires when ASP.NET chooses to
        // bind, not from an explicit user action. That's objectively
        // less certain evidence of "change in handler X affects this
        // control" than a direct `control → function` hop (EventWiring
        // via OnClick / Handles). Penalise DataBinding hops so callers
        // reading the provenance see the weaker paths ranked lower.
        //
        // Per-hop penalty table (subtracted from a starting confidence
        // of 1.0, then floored at 0.1 so a wholly declarative chain
        // still reports something non-zero):
        //   control→control   -0.25  (DataBinding — declarative)
        //   page→control      -0.05  (Contains — structural)
        //   control→function  -0.00  (EventWiring — direct)
        //   function→function -0.05  (Calls — indirect but typed)
        //   *→inline_sql      -0.00  (terminal evidence)
        //   *→stored_proc     -0.00  (terminal evidence)
        //   *→*               -0.10  (other / unknown)
        let path_scores: Vec<f64> = paths
            .iter()
            .map(|p| path_confidence_score(p, fallback_penalty))
            .collect();
        let best_path_score = path_scores
            .iter()
            .copied()
            .fold(0.0f64, |a, b| if a > b { a } else { b });

        out.push_str("\n## Trace Provenance\n");
        out.push_str(&format!("trace_used_fallback: {}\n", trace_used_fallback));
        out.push_str(&format!(
            "trace_candidate_count: {}\n",
            trace_candidate_count
        ));
        out.push_str(&format!(
            "trace_confidence_penalty_fallback: {:.2}\n",
            fallback_penalty
        ));
        out.push_str(&format!("best_path_confidence: {:.2}\n", best_path_score));
        out.push_str(&format!("selected_start_node: {}\n", start_id));

        if trace_used_fallback {
            out.push_str(&format!(
                "\n### Ambiguity Warning\n\
                 Control lookup used fallback candidate matching ({} candidates found).\n\
                 Penalty reason: {} candidate(s) matched control ID filter; first-match selected.\n\
                 Risk: Incorrect handler resolution may lead to wrong trace path.\n",
                trace_candidate_count, trace_candidate_count
            ));
            out.push_str("\n### Unresolved Candidates\n");
            for (i, cand) in unresolved_candidates.iter().enumerate() {
                let selected = if i == 0 { " ← SELECTED" } else { "" };
                out.push_str(&format!("  {}. {}{}\n", i + 1, cand, selected));
            }
        }

        // Low-confidence paths — emit actionable follow-up probes so the
        // caller can tighten the trace rather than accepting the
        // ambiguous result at face value. Fires for BOTH fallback
        // resolution AND paths that cross a DataBinding hop.
        let has_data_binding_hop = paths.iter().any(|p| {
            p.windows(2)
                .any(|w| w[0].node_type == "control" && w[1].node_type == "control")
        });
        if trace_used_fallback || has_data_binding_hop || best_path_score < 0.75 {
            out.push_str("\n### Follow-up Probes\n");
            if trace_used_fallback {
                out.push_str("- Provide explicit `handler_fqn` to disambiguate control lookup\n");
                out.push_str("- Verify control ID uniqueness across master / user controls\n");
                out.push_str("- Check code-behind inheritance chain for handler shadowing\n");
            }
            if has_data_binding_hop {
                out.push_str(
                    "- Path crosses a declarative data-binding hop (control → control). \
                     Run `trace_data_flow` on the data-source control's event handler for \
                     deeper SQL tracing\n",
                );
                out.push_str(
                    "- Run `find_symbol_references` on the resolved handler method to find \
                     alternate call paths that bypass this trace\n",
                );
            }
            if best_path_score < 0.75 && !trace_used_fallback && !has_data_binding_hop {
                out.push_str(
                    "- Best path confidence below 0.75. Consider narrowing with a more \
                     specific `handler_fqn` or re-running `trace_ui_action` against the \
                     button that initiates this flow\n",
                );
            }
        }

        for (i, path) in paths.iter().enumerate() {
            let score = path_scores.get(i).copied().unwrap_or(0.0);
            let confidence_tag = if score >= 0.85 {
                "high"
            } else if score >= 0.6 {
                "medium"
            } else {
                "low"
            };
            out.push_str(&format!(
                "\n## Path #{} (confidence {:.2} — {})\n",
                i + 1,
                score,
                confidence_tag
            ));
            for (step, node) in path.iter().enumerate() {
                let label = match node.node_type.as_str() {
                    "page" => "ASPX Page",
                    "control" => "UI Control",
                    "function" => "Code-Behind Handler",
                    "stored_proc" => "Stored Procedure",
                    "inline_sql" => "Inline SQL",
                    _ => &node.node_type,
                };

                let (justification, hop_kind) = if step == 0 {
                    ("Starting point".to_string(), "start")
                } else {
                    let prev = &path[step - 1];
                    match (prev.node_type.as_str(), node.node_type.as_str()) {
                        ("page", "class") => ("Inherits class".to_string(), "inherits"),
                        ("page", "control") => ("Contains control".to_string(), "contains"),
                        ("control", "control") => (
                            "Declarative data binding (DataSourceID)".to_string(),
                            "data_binding",
                        ),
                        ("control", "function") => (
                            "Event wiring (OnClick / Handles)".to_string(),
                            "event_wiring",
                        ),
                        ("function", "function") => ("Method call".to_string(), "calls"),
                        (_, "inline_sql") | (_, "stored_proc") => {
                            ("Executes SQL".to_string(), "sql_terminal")
                        }
                        _ => ("Dependency".to_string(), "dependency"),
                    }
                };

                let evidence = format!(
                    "node_type={}, hop_kind={}, file={}, lines={}-{}",
                    node.node_type,
                    hop_kind,
                    node.file_path.as_str(),
                    node.start_line,
                    node.end_line
                );

                // Prefer the metadata FQN for display: node IDs are
                // location-based, but humans and agents identify handlers by
                // their fully-qualified name.
                let display_name = node
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("fqn"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(&node.name);

                let indent = "  ".repeat(step);
                out.push_str(&format!(
                    "{indent}Step {}: {} [{}] ({}) - {} | evidence: {}\n",
                    step + 1,
                    display_name,
                    label,
                    node.node_id,
                    justification,
                    evidence
                ));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_trace_ui_action(
        &self,
        req: TraceUiActionRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let (gen_, _source_snapshot) = self.pin_active_source_generation(&req.project_id, &ps.search).await?;
        #[cfg(test)]
        crate::handlers::search_tools::selected_source_tests::after_selection(&req.project_id).await;

        let hits = ps
            .search
            .search(
                &HybridQuery {
                    project_id: req.project_id.clone(),
                    namespace: "memory".into(),
                    generation: gen_,
                    text: req.query.clone(),
                    top_k: 10,
                    fts_mode: "loose".into(),
                    include_path_prefixes: None,
                    exclude_path_prefixes: None,
                    include_path_suffixes: None,
                    language_filters: None,
                    author_filter: None,
                    date_after: None,
                    date_before: None,
                    use_mmr: false,
                },
                None,
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let mut start_nodes = Vec::new();
        for h in &hits {
            let nodes = self
                .state
                .graph
                .query_nodes(&req.project_id, None, None, Some(h.path.as_str()), 10)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;

            for n in nodes {
                if matches!(n.node_type.as_str(), "control" | "page" | "function") {
                    start_nodes.push(n.node_id);
                }
            }
        }
        start_nodes.sort();
        start_nodes.dedup();

        if start_nodes.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No UI controls or handlers found for the query.",
            )]));
        }

        let mut out = format!("UI Trace results for '{}':\n", req.query);
        let mut paths_found = 0;

        let edge_kinds = vec![
            engram_graph::EdgeKind::Contains,
            engram_graph::EdgeKind::Dependency,
            // Raw `calls` edges map to their own kind (restored through the
            // ingest pipeline) — without it the trace stops at the handler
            // and never shows downstream calls.
            engram_graph::EdgeKind::Calls,
        ];

        for start_id in start_nodes {
            if paths_found >= req.sanitized_max_paths() {
                break;
            }

            let paths = self
                .state
                .graph
                .traverse(
                    &req.project_id,
                    &start_id,
                    req.sanitized_max_depth(),
                    Some(edge_kinds.clone()),
                    "out",
                )
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;

            if paths.len() > 1 {
                paths_found += 1;
                out.push_str(&format!("\nPath starting at {}:\n", start_id));
                for (n, depth) in paths {
                    let indent = "  ".repeat(depth);
                    out.push_str(&indent);
                    out.push_str(&format!(
                        "- {} | {} | {} (lines {}-{})\n",
                        n.node_id, n.node_type, n.file_path, n.start_line, n.end_line
                    ));
                }
            }
        }

        if paths_found == 0 {
            return Ok(CallToolResult::success(vec![Content::text(
                "No call chains found from identified UI elements.",
            )]));
        }

        Ok(CallToolResult::success(vec![Content::text(
            out.trim().to_string(),
        )]))
    }

    pub async fn handle_export_capture_pack(
        &self,
        req: ExportCapturePackRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let pid = req.project_id.clone();
        let _ps = self.ensure_project_runtime(&pid).await?;

        // Fetch overview text before entering spawn_blocking (it's async)
        let overview_result = self
            .handle_get_codebase_overview(ProjectIdRequest {
                project_id: pid.clone(),
            }.into())
            .await?;
        let overview_text = overview_result
            .content
            .first()
            .and_then(|c| {
                if let rmcp::model::RawContent::Text(t) = &c.raw {
                    Some(t.text.clone())
                } else {
                    None
                }
            })
            .unwrap_or_default();

        // Stream the zip directly to disk to avoid OOM for large projects.
        let timestamp = now_ms();
        let data_dir = self.state.cfg.data_dir.clone();
        let exports_dir = data_dir.join("exports").join(&pid);
        tokio::fs::create_dir_all(&exports_dir)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let zip_path = exports_dir.join(format!("{}.zip", timestamp));

        let graph = self.state.graph.clone();
        let pid_clone = pid.clone();
        let zip_path_clone = zip_path.clone();
        // Memory sections travel with the pack — otherwise they die with
        // registry.redb and the pack is not a real backup. Restored with
        // import_memory_bank.
        let mem_sections = self
            .state
            .registry
            .list_memory_sections(&pid)
            .unwrap_or_default();
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let file = std::fs::File::create(&zip_path_clone).map_err(|e| e.to_string())?;
            let writer = std::io::BufWriter::new(file);
            let mut zip = zip::ZipWriter::new(writer);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);

            // 1. overview.md
            zip.start_file("overview.md", options)
                .map_err(|e| e.to_string())?;
            std::io::Write::write_all(&mut zip, overview_text.as_bytes())
                .map_err(|e| e.to_string())?;

            // 2. graph_topology.json
            let all_nodes = graph
                .query_nodes(&pid_clone, None, None, None, 1000)
                .unwrap_or_default();
            let total_node_count = graph.count_nodes(&pid_clone).unwrap_or(all_nodes.len());
            let topo = serde_json::json!({
                "node_count": total_node_count,
                "nodes": all_nodes.iter().map(|n| {
                    serde_json::json!({
                        "id": n.node_id,
                        "type": n.node_type,
                        "name": n.name,
                        "path": n.file_path,
                        "language": n.language
                    })
                }).collect::<Vec<_>>()
            });
            zip.start_file("graph_topology.json", options)
                .map_err(|e| e.to_string())?;
            serde_json::to_writer_pretty(&mut zip, &topo).map_err(|e| e.to_string())?;

            // 3. ui_wiring.json
            let ui_nodes = graph
                .query_nodes(&pid_clone, Some("control"), None, None, 5000)
                .unwrap_or_default();
            let mut wiring = Vec::new();
            for ctrl in ui_nodes {
                let deps = graph
                    .neighbors(&pid_clone, EdgeKind::Dependency, &ctrl.node_id, 10)
                    .unwrap_or_default();
                wiring.push(serde_json::json!({
                    "control": ctrl.node_id,
                    "handlers": deps.iter().map(|(id, _)| id).collect::<Vec<_>>()
                }));
            }
            zip.start_file("ui_wiring.json", options)
                .map_err(|e| e.to_string())?;
            serde_json::to_writer_pretty(&mut zip, &wiring).map_err(|e| e.to_string())?;

            // 4. sql_map.json
            let sql_edges = graph
                .list_edges_by_kind(&pid_clone, EdgeKind::SqlCalls, 5000)
                .unwrap_or_default();
            zip.start_file("sql_map.json", options)
                .map_err(|e| e.to_string())?;
            serde_json::to_writer_pretty(&mut zip, &sql_edges).map_err(|e| e.to_string())?;

            // 5. memory/{section_id}.md — the portable, restorable form.
            for sec in &mem_sections {
                let safe = sec.section_id.replace(['/', '\\', '\0'], "_");
                zip.start_file(format!("memory/{safe}.md"), options)
                    .map_err(|e| e.to_string())?;
                let md = crate::services::memory_portability::to_markdown(sec);
                std::io::Write::write_all(&mut zip, md.as_bytes()).map_err(|e| e.to_string())?;
            }

            zip.finish().map_err(|e| e.to_string())?;
            Ok(())
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "\u{2705} Capture pack exported to: {}",
            zip_path.to_string_lossy()
        ))]))
    }

    pub async fn handle_get_ui_blueprint(
        &self,
        req: GetUiBlueprintRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let _ = self.ensure_project_runtime(&req.project_id).await?;

        let graph = self.state.graph.clone();
        let out = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let all_containers = graph
                .query_nodes(&req.project_id, Some("ui_container"), None, Some(&req.file_path), 500)
                .map_err(|e| e.to_string())?;

            if all_containers.is_empty() {
                return Ok(format!(
                    "No UI layout data found for '{}'. Ensure the file has been indexed and contains container elements (Panel, Table, GroupBox, div).",
                    req.file_path
                ));
            }

            let mut tree = serde_json::Map::new();
            tree.insert("file".into(), serde_json::Value::String(req.file_path.clone()));

            let mut containers_json = Vec::new();
            for container in &all_containers {
                let mut cobj = serde_json::Map::new();
                cobj.insert("id".into(), serde_json::Value::String(container.name.clone()));
                cobj.insert("node_id".into(), serde_json::Value::String(container.node_id.clone()));

                if let Some(ref meta) = container.metadata {
                    for key in ["container_type", "layout_style", "logical_grouping", "css_class"] {
                        if let Some(val) = meta.get(key).and_then(|v| v.as_str()) {
                            cobj.insert(key.into(), serde_json::Value::String(val.to_string()));
                        }
                    }
                }

                let children = graph
                    .neighbors(&req.project_id, EdgeKind::ContainsUi, &container.node_id, 200)
                    .unwrap_or_default();

                let mut children_json = Vec::new();
                for (child_id, _weight) in &children {
                    let mut child_obj = serde_json::Map::new();
                    child_obj.insert("node_id".into(), serde_json::Value::String(child_id.clone()));

                    if let Ok(Some(child_node)) = graph.get_node(&req.project_id, child_id) {
                        child_obj.insert("name".into(), serde_json::Value::String(child_node.name.clone()));
                        child_obj.insert("type".into(), serde_json::Value::String(child_node.node_type.clone()));

                        if let Some(ref meta) = child_node.metadata {
                            for key in ["ui_label", "row", "col", "logical_grouping", "x", "y"] {
                                if let Some(val) = meta.get(key).and_then(|v| v.as_str()) {
                                    child_obj.insert(key.into(), serde_json::Value::String(val.to_string()));
                                }
                            }
                        }

                        let neighbors = graph
                            .neighbors(&req.project_id, EdgeKind::UiLayoutNeighbor, child_id, 5)
                            .unwrap_or_default();
                        if !neighbors.is_empty() {
                            let next_ids: Vec<serde_json::Value> = neighbors
                                .iter()
                                .map(|(nid, _)| serde_json::Value::String(nid.clone()))
                                .collect();
                            child_obj.insert("next_in_tab_order".into(), serde_json::Value::Array(next_ids));
                        }
                    }
                    children_json.push(serde_json::Value::Object(child_obj));
                }

                cobj.insert("children".into(), serde_json::Value::Array(children_json));
                containers_json.push(serde_json::Value::Object(cobj));
            }

            tree.insert("containers".into(), serde_json::Value::Array(containers_json));
            tree.insert("container_count".into(), serde_json::Value::Number(all_containers.len().into()));
            serde_json::to_string_pretty(&tree).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_get_codebase_overview(
        &self,
        req: crate::models::CodebaseOverviewRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        if req.source_contains.as_deref().is_some_and(|term|term.is_empty() || term.len()>128
            || term.chars().any(char::is_control) || !req.include_source || req.include_declarations
            || req.symbol_name.is_some() || req.symbol_node_id.is_some()) {
            return Err(McpError::invalid_params("source_contains requires literal text1..128UTF8bytes without controls, include_source=true, include_declarations=false and no symbol selectors; it inspects one selected file, not owners or calls",None));
        }
        if req.include_source && (req.scope.is_none() || req.offset != 0
            || req.symbol_offset != 0 || req.include_retained_context) {
            return Err(McpError::invalid_params("include_source requires one exact selected-file scope, offsets0 and include_retained_context=false", None));
        }
        let valid = |path: &str| !path.is_empty() && path.len()<=2048
            && !path.contains(['\\',':','\0','\n','\r'])
            && path.split('/').all(|part| !part.is_empty() && part!="." && part!="..");
        if !(1..=64).contains(&req.limit) || req.offset>250_000 || req.symbol_offset>128
            || (!req.include_declarations && (req.symbol_offset!=0 || req.symbol_node_id.is_some() || req.symbol_name.is_some()))
            || (req.symbol_offset>0 && (req.scope.is_none() || req.offset!=0)) || req.exclude_paths.len()>16
            || req.symbol_node_id.as_deref().is_some_and(|id|id.is_empty() || id.len()>2048
                || id.chars().any(char::is_control) || req.scope.is_none() || req.offset!=0 || req.symbol_offset!=0)
            || req.symbol_name.as_deref().is_some_and(|name|name.is_empty() || name.len()>256
                || name.chars().any(char::is_control) || req.scope.is_none()
                || req.symbol_offset!=0 || req.symbol_node_id.is_some())
            || req.exclude_paths.iter().map(String::len).sum::<usize>()>4096
            || !req.exclude_paths.iter().all(|path|valid(path))
            || req.path_contains.as_deref().is_some_and(|term|term.is_empty() || term.len()>128
                || term.chars().any(char::is_control))
            || req.scope.as_deref().is_some_and(|scope|!valid(scope)) {
            return Err(McpError::invalid_params("Use literal relative scope/exclusions, optional nonempty path_contains up to128bytes without controls, limit1..64 and bounded offsets; include_declarations=false requires symbol_offset0 and no symbol selectors. symbol_node_id requires exact-file scope and offsets0. symbol_name requires exact-file offset0 or a directory page (limit1..8, no retained context), symbol_offset0 and no symbol_node_id; exact case-sensitive extracted name1..256bytes without controls, not class/FQN resolution",None));
        }
        let rec=self.ensure_project_record(&req.project_id).await?;
        let runtime=self.ensure_project_runtime(&req.project_id).await?;
        let (generation,view)=self.pin_active_source_generation(&req.project_id,&runtime.search).await?;
        let source_hold=std::sync::Arc::new(view);
        let managed_source = source_hold.is_managed();
        let search=runtime.search.clone();
        let graph=self.state.graph.clone();
        let worker_view=source_hold.clone();
        let worker_request=req.clone();
        let (summary,paths,file_rows,discovery,retained,compact_navigation,exact_file,directory_named)=tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let pid=&worker_request.project_id;
            let membership=search.selected_source_revision_lookup(pid,generation,worker_view.clone())?;
            let paths=membership.navigation_paths(worker_request.scope.as_deref(),&worker_request.exclude_paths,
                worker_request.offset,worker_request.limit,worker_request.path_contains.as_deref())?;
            let first_page=if worker_request.symbol_name.is_some() && worker_request.offset>0 {
                Some(membership.navigation_paths(worker_request.scope.as_deref(),&worker_request.exclude_paths,
                    0,1,worker_request.path_contains.as_deref())?)
            }else{None};
            let scope_paths=first_page.as_ref().unwrap_or(&paths);
            let exact_file=worker_request.scope.as_deref().is_some_and(|scope| scope_paths.files.len()==1
                && if cfg!(windows){scope_paths.files[0].0.eq_ignore_ascii_case(scope)}else{scope_paths.files[0].0==scope});
            if worker_request.include_source && !exact_file {
                return Ok((None,paths,Vec::new(),Vec::new(),None,true,false,
                    Some(Err(McpError::invalid_params("include_source requires an exact selected file surviving scope, filter and exclusions",None)))));
            }
            anyhow::ensure!(worker_request.symbol_offset==0 || exact_file,
                "symbol_offset requires an exact selected-file scope and offset0");
            anyhow::ensure!(worker_request.symbol_node_id.is_none() || exact_file,
                "symbol_node_id requires an exact selected-file scope and both offsets0");
            if worker_request.symbol_name.is_some() {
                if exact_file {
                    if worker_request.offset!=0 {
                        return Ok((None,paths,Vec::new(),Vec::new(),None,true,true,
                            Some(Err(McpError::invalid_params("Exact-file symbol_name requires offset0",None)))));
                    }
                }else{
                    anyhow::ensure!(!paths.files.is_empty() || paths.matching_files.is_some_and(|count|count>0),
                        "symbol_name requires a selected-file or managed directory boundary; unavailable coverage is not absence");
                    anyhow::ensure!(worker_request.limit<=8 && !worker_request.include_retained_context,
                        "Directory symbol_name requires limit1..8 and no retained context");
                    let files:Vec<_>=paths.files.iter().map(|file|file.0.as_str()).collect();
                    let admit=|node:&engram_graph::Node,file:Option<&engram_graph::Node>|
                        crate::services::impact_walk_service::admit_selected_source(&membership,node,file);
                    let (nodes,coverage)=graph.bounded_files_named_nodes(pid,&files,
                        worker_request.symbol_name.as_deref().unwrap(),&admit)?;
                    let navigation=directory_named_navigation(&worker_request,&paths,nodes,coverage,generation,worker_view.is_managed())?;
                    return Ok((None,paths,Vec::new(),Vec::new(),None,true,false,Some(Ok(navigation))));
                }
            }
            let compact_navigation=(!worker_request.include_declarations || exact_file || worker_request.path_contains.is_some()) && !worker_request.include_retained_context;
            let follow=|action|codemap_followup(action,worker_request.path_contains.as_deref());
            let summary=if compact_navigation { None }
                else { Some(search.selected_source_summary(pid,generation,worker_view)?) };
            let mut rows=Vec::new();
            let mut discovery=Vec::new();
            for batch in paths.files.chunks(8) {
                if !worker_request.include_declarations {
                    for (path,revision,_) in batch {
                        rows.push(serde_json::json!({"path":path,"physical_generation":revision,
                            "actions":[follow(serde_json::json!({"tool":"get_codebase_overview","arguments":{
                                "project_id":pid,"scope":path,"exclude_paths":worker_request.exclude_paths,
                                "include_declarations":true}})),
                                {"tool":"get_full_method_body","arguments":{"project_id":pid,"file_path":path,
                                    "context_lines":0,"output_json":true}}]}));
                    }
                    continue;
                }
                let files:Vec<_>=batch.iter().map(|(path,_,_)|path.clone()).collect();
                let admit=|node:&engram_graph::Node,file:Option<&engram_graph::Node>|
                    crate::services::impact_walk_service::admit_selected_source(&membership,node,file);
                let observed = if let Some(id) = worker_request.symbol_node_id.as_deref() {
                    let (nodes, observation) = exact_navigation_node(&graph, &membership, pid, &files[0], id);
                    discovery.push(observation);
                    nodes
                } else if let Some(name) = worker_request.symbol_name.as_deref() {
                    let (nodes, observation) = graph.bounded_file_named_nodes(pid, &files[0], name, &admit)?;
                    discovery.push(observation);
                    nodes
                } else {
                    let (seeds,seed_coverage)=graph.bounded_file_navigation_seeds(pid,&files,&admit)?;
                    let walk=if seeds.is_empty(){engram_graph::store::IncomingImpactWalk::default()}
                        else{graph.bounded_incoming_walk(pid,&seeds,1,&|_|false,&admit)?};
                    discovery.push(serde_json::json!({"seed_discovery":seed_coverage,"declaration_admission":walk.coverage}));
                    walk.nodes.into_iter().filter(|item|
                        item.admission==engram_graph::store::ImpactAdmission::Selected).map(|item|item.node).collect()
                };
                for (path,revision,area) in batch {
                    let mut nodes:Vec<_>=observed.iter().filter(|node|
                        node.node_type!="file" && node.file_path.as_str()==path).collect();
                    let callable=|kind:&str|matches!(kind,"function"|"method"|"constructor"|"property"|"sub"|"procedure");
                    nodes.sort_by(|a,b|(!callable(&a.node_type)).cmp(&(!callable(&b.node_type)))
                        .then_with(||a.name.cmp(&b.name)).then_with(||a.node_id.cmp(&b.node_id)));
                    let selected=worker_request.symbol_node_id.as_deref();
                    let named=worker_request.symbol_name.as_deref();
                    let targeted=selected.is_some() || named.is_some();
                    let cap=if named.is_some(){8}else if exact_file && !targeted{16}else{1};
                    let symbol_offset=if targeted{0}else if exact_file{worker_request.symbol_offset}else{0};
                    let symbols:Vec<_>=nodes.iter().skip(symbol_offset).take(cap).map(|item|{
                        let node=*item;
                        let mut actions=Vec::new();
                        if node.start_line>0 && node.end_line>=node.start_line {
                            actions.push(serde_json::json!({"tool":"get_full_method_body","arguments":{
                                "project_id":pid,"file_path":path,"line_start":node.start_line,
                                "line_end":node.end_line.min(node.start_line.saturating_add(119)),
                                "context_lines":0,"output_json":true}}));
                        }
                        if targeted {
                            actions.push(serde_json::json!({"tool":"impact_analysis","arguments":{
                                "project_id":pid,"symbol_fqn":node.node_id,"max_depth":6,"limit":12,"detail":"summary"}}));
                        }
                        if targeted && node.start_line>0 && matches!(node.node_type.as_str(),"function"|"method"|"sub"|"procedure") {
                            let mut args=serde_json::json!({"project_id":pid,"file_path":path,
                                "method_name":node.name.rsplit('.').next().unwrap_or(&node.name),"line":node.start_line,
                                "include_history":false,"include_business_logic":false,"include_full_body":false,"include_caller_bodies":false,
                                "max_callers":2,"output_json":true});
                            if let Some((prefix,_))=node.name.rsplit_once('.') {
                                args["class_name"]=serde_json::json!(prefix.rsplit('.').next().unwrap_or(prefix));
                            }
                            actions.push(serde_json::json!({"tool":"get_method_edit_context","arguments":args}));
                        }
                        serde_json::json!({"node_id":node.node_id,"name":node.name,"kind":node.node_type,
                            "start_line":node.start_line,"end_line":node.end_line,"generation":node.generation,
                            "actions":actions})
                    }).collect();
                    let next_offset=symbol_offset.saturating_add(symbols.len());
                    let next_symbols=if exact_file && !targeted && next_offset<nodes.len() && next_offset<=128 {
                        follow(serde_json::json!({"tool":"get_codebase_overview","arguments":{"project_id":pid,"scope":path,
                            "exclude_paths":worker_request.exclude_paths,"offset":0,"limit":1,"symbol_offset":next_offset}}))
                    }else{serde_json::Value::Null};
                    let mut file_actions=vec![follow(serde_json::json!({"tool":"get_codebase_overview","arguments":{"project_id":pid,"scope":path,
                        "exclude_paths":worker_request.exclude_paths,"offset":0,"limit":1}}))];
                    if !symbols.iter().any(|symbol|symbol["actions"].as_array().is_some_and(|actions|
                        actions.iter().any(|action|action["tool"]=="get_full_method_body"))) {
                        file_actions.push(serde_json::json!({"tool":"get_full_method_body","arguments":{
                            "project_id":pid,"file_path":path,"context_lines":0,"output_json":true}}));
                    }
                    let mut row=serde_json::json!({"path":path,"physical_generation":revision,"area":area,"symbols":symbols,
                        "symbol_offset":symbol_offset,"symbol_display_limited":if targeted{symbols.len()<nodes.len()}else{next_offset<nodes.len()},
                        "declarations_observed":nodes.len(),"next_symbols":next_symbols,
                        "actions":file_actions});
                    if let Some(id)=selected {
                        row["symbol_selection"]=serde_json::json!({"node_id":id,"observed":!symbols.is_empty(),
                            "scope":"exact_bounded_identity_lookup; not observed does not prove absent"});
                        row["declaration_inventory"]=serde_json::json!("not_scanned");
                    }
                    if let Some(name)=named {
                        row["declaration_inventory"]=serde_json::json!("not_scanned");
                        row["name_selection"]=serde_json::json!({"requested_name":name,
                            "match":"exact_case_sensitive_extracted_name","observed":!symbols.is_empty(),
                            "scope":"Admitted bounded native callable candidates, not uniqueness, FQN resolution or absence proof"});
                        if let Some(lookup)=discovery.last().filter(|lookup|
                            lookup["name_match"]=="exact_case_sensitive_terminal_segment") {
                            row["name_selection"]["match"]=lookup["name_match"].clone();
                            row["name_selection"]["exact_extracted_name_priority"]=serde_json::json!(true);
                            row["declaration_inventory"]=lookup["declaration_inventory"].clone();
                        }
                        // Fit candidates inside the sole file row so crowding
                        // cannot discard every matched identity and action.
                        let original=symbols.len();
                        loop {
                            named_source_fallback(&mut row,pid);
                            if serde_json::to_vec(&row)?.len()<=40*1024 || row["symbols"].as_array().unwrap().len()<=1 {break;}
                            row["symbols"].as_array_mut().unwrap().pop();
                        }
                        let shown=row["symbols"].as_array().unwrap().len();
                        row["symbols_omitted_from_display"]=serde_json::json!(original-shown);
                        row["symbol_display_limited"]=serde_json::json!(shown<nodes.len()
                            || discovery.last().is_some_and(|item|item["declaration_admission"]["truncated"]==true));
                        anyhow::ensure!(serde_json::to_vec(&row)?.len()<=48*1024,
                            "Named candidate metadata exceeds response bound; narrow name/path");
                    }
                    rows.push(row);
                }
            }
            let retained=if worker_request.include_retained_context {
                Some((search.count_docs(pid)?,search.count_docs_by_namespace(pid)?,graph.insight_title_sample(pid,8)))
            }else{None};
            Ok((summary,paths,rows,discovery,retained,compact_navigation,exact_file,None))
        }).await.map_err(|e|McpError::internal_error(e.to_string(),None))?
            .map_err(|e|McpError::internal_error(format!("CodeMap navigation unavailable: {e:#}"),None))?;
        if let Some(navigation)=directory_named {
            let navigation=navigation?;
            let encoded=serde_json::to_string(&navigation).map_err(|e|McpError::internal_error(e.to_string(),None))?;
            let mut out=format!("Codebase Overview: {}\nproject_id: {}\nproject_type: {}\nactive_generation: {}\n\n## CodeMap Navigation\n\n```json\n{}\n```\n",
                rec.project_name,req.project_id,rec.project_type,generation,encoded);
            out.push_str(&self.freshness_footer(&req.project_id,generation).await);
            return Ok(CallToolResult::success(vec![Content::text(out)]));
        }
        let explore=|scope:&str,offset:usize|{
            let mut action=codemap_followup(serde_json::json!({"tool":"get_codebase_overview","arguments":{
                "project_id":req.project_id,"scope":scope,"exclude_paths":req.exclude_paths,"offset":offset,"limit":req.limit}}),req.path_contains.as_deref());
            if !req.include_declarations {action["arguments"]["include_declarations"]=serde_json::json!(false);}
            action
        };
        let file_selection_scope=if req.path_contains.is_some() {
            "Filtered selected paths page lexically; the literal case-insensitive substring is applied with scope/exclusions before bounded declaration discovery. This is not a semantic or source-body search."
        }else{"Unfiltered root areas are immediate path children with counts for their complete boundary within the observed selection; follow their literal actions for deeper areas. Root files remain diverse representatives from deeper groups (files[].area), independently bounded in representative_groups_truncated. Scoped area actions descend to the next child directory or direct file; scoped file pages retain lexical order and page offsets recover selected files beyond the page."};
        let mut navigation=serde_json::json!({"format":"engram-codemap-navigation-v1","source_generation":generation,
            "scope":req.scope,"exclude_paths":req.exclude_paths,"working_tree_checked":false,
            "coverage":{"managed_source":managed_source,"matching_files":paths.matching_files,
                "inspected_file_identities":paths.inspected_files,"scan_truncated":paths.scan_truncated,
                "area_sample_truncated":paths.areas_truncated,"representative_sample":paths.representative_sample,
                "representative_priority":"Generic Policy, Service/Controller/Handler, Application/Bootstrap/Main basename leads before area size, then path; bounded to retained groups, not runtime architecture.",
                "declarations":discovery},"areas":[],"files":[],"next_page":null,
            "other_inventories":[{"label":"Database schema/config inventory not scanned by CodeMap","tool":"query_graph_nodes",
                "arguments":{"project_id":req.project_id,"node_type":"db_table","limit":20}},
                {"label":"Settings/config inventory not scanned by CodeMap","tool":"list_settings",
                "arguments":{"project_id":req.project_id,"max_per_category":10}}],
            "limitations":["Directory groups describe source layout, not runtime architecture or accepted policy.",
                "File identities belong to one held published source selection, including inherited revisions. Declarations are bounded live-graph observations admitted against that selection.",
                "No filesystem byte/LOC scan, source-body read, whole-project graph scan, native vector table or model query was performed.",
                file_selection_scope,
                "Missing declarations or an empty bounded sample do not prove absence of code, consumers or entry points. Legacy file coverage is unknown.",
                "Root/area previews show one declaration; exact-file pages show16 observed declarations with source-range actions and next_symbols (offset0..128). Set symbol_node_id to a known identity under the exact-file scope with both offsets0 for independently bounded target/file admission and that declaration's full actions, without declaration discovery or name fallback. Selection is a fresh observation: compare source_generation and coverage; it is not atomic continuation. Not observed does not prove absence.",
                "Ranges are indexed positions capped to120lines; follow-up verifies current bytes, not full methods or runtime entry points. Declaration discovery coverage is reported once above."]});
        navigation["limitations"].as_array_mut().expect("limitations array").push(serde_json::json!(
            "If no displayed declaration offers a source range, the file action offers a bounded current-file excerpt. This reads current bytes only when executed; compare page hashes and do not infer method boundaries or indexed freshness."));
        if req.scope.is_none() && req.path_contains.is_none() {
            navigation["coverage"]["representative_groups_truncated"]=serde_json::json!(paths.representative_groups_truncated);
        }
        if let Some(term)=req.path_contains.as_deref() {
            navigation["path_contains"]=serde_json::json!(term);
            navigation["coverage"].as_object_mut().expect("coverage object").remove("representative_priority");
            navigation["coverage"]["file_order"]=serde_json::json!("lexical_filtered_paths");
        }
        if compact_navigation {
            navigation["detail"] = serde_json::json!(if req.symbol_name.is_some(){"exact_name_candidates"}else if req.symbol_node_id.is_some(){"exact_symbol"}else if exact_file{"exact_file"}else{"filtered_paths"});
            navigation.as_object_mut().expect("navigation object").remove("other_inventories");
            navigation["coverage"].as_object_mut().expect("coverage object").remove("representative_priority");
            navigation["limitations"] = serde_json::json!([
                "One held published source selection, including inherited physical revisions; declarations are bounded admitted graph observations.",
                "Not observed is not absent. Discovery/admission/scan gaps are reported in coverage; no current-source, binding or runtime verification.",
                "Indexed ranges are capped at120lines. Follow-ups observe fresh state/current bytes; compare source_generation and hashes, not complete-method or entry-point proof.",
                "File reads without line bounds are bounded current-file excerpts, not indexed method boundaries. Compare their hashes and continuation.",
                "File actions page admitted declarations (16 per page, bounded through offset128). Exact symbol_node_id uses independent bounded target/file admission, not a declaration inventory. Project distributions and area/schema/settings context omitted; use root/area overview or request include_retained_context=true when that context is needed."
            ]);
            if !exact_file {
                navigation["limitations"]=serde_json::json!([
                    "Literal case-insensitive path substring over one held selected file map, applied with scope/exclusions before bounded declaration lookup; not a source-body or semantic search.",
                    "Filtered files page lexically. Area/group caps do not limit independent file pagination. Scan/legacy/declaration gaps remain in coverage; a missing file or declaration is not proof of absent code or consumers.",
                    "Declarations are bounded admitted graph observations, with one preview per file. Follow exact-file/known-symbol actions for more; indexed ranges are capped at120lines. Current bytes, freshness, binding and runtime are unverified.",
                    "Filter and exclusions persist in CodeMap actions. Follow-ups observe fresh selections, not atomic continuations. Project-wide distributions and retained context omitted; include_retained_context=true restores them with their unfiltered denominators."
                ]);
            }
        }
        if req.symbol_name.is_some() {
            navigation["detail"]=serde_json::json!("exact_name_candidates");
            navigation["limitations"]=serde_json::json!([
                "Exact case-sensitive extracted Node.name within one selected canonical file; not class/FQN/suffix resolution. For example use save, not Widget.save when the extracted name is save.",
                "Bounded native function/method/constructor/property prefixes under the held file/node selection. Same-name declarations remain candidates, not a unique target. Legacy/noncanonical identities, other kinds and name aliases remain unassessed; not observed never proves absence.",
                "Lookup work/decode/candidate/deadline gaps are reported in coverage. Display may omit retained candidates; no offset-based inventory continuation. File action returns ordinary discovery; symbol_node_id expands a known identity.",
                "Executable source/impact/context actions are fresh observations, not atomic replay. Compare returned generations/hashes; indexed ranges are bounded and current-source, method-boundary, binding and runtime proof remain separate."
            ]);
            if navigation["coverage"]["declarations"].as_array().is_some_and(|lookups|
                lookups.iter().any(|lookup|lookup["name_match"]=="exact_case_sensitive_terminal_segment")) {
                navigation["limitations"][0]=serde_json::json!(
                    "No admitted exact extracted-name candidate was observed. A bare name then compares exact case-sensitive final dot-separated name segments within this selected file; full qualified identities and overload candidates remain distinct, not resolved bindings.");
                navigation["limitations"][1]=serde_json::json!(
                    "Exact seeks and callable file-prefix recovery share one key/decode/candidate/deadline budget and held file/node selection. A capped or empty result does not prove absence. Exact-name successes are not unioned with qualified terminal alternatives; directory-name pages remain exact extracted-name only.");
            }
        }
        if !req.include_declarations {
            navigation["detail"]=serde_json::json!("paths_only");
            navigation["include_declarations"]=serde_json::json!(false);
            navigation["coverage"].as_object_mut().expect("coverage object").remove("declarations");
            navigation["coverage"]["declaration_lookup"]=serde_json::json!("not_requested");
            navigation.as_object_mut().expect("navigation object").remove("other_inventories");
            navigation["limitations"]=serde_json::json!([
                "Held selected file identities/revisions only; declarations, current existence and bytes unchecked. Skipped declarations are unknown.",
                "Explore/page keep mode, filter and exclusions; browse clears only the filter. use_file_actions areas share the matching displayed file's literal declaration/source actions.",
                "Follow-ups are fresh, not atomic. Compare generation/hash; excerpts are not method bounds. Sample/scan/legacy gaps stay in coverage; retained context is separate."
            ]);
        }
        let mut areas=Vec::new();
        for (area,count) in paths.areas.iter().filter(|_| !exact_file || !compact_navigation) {
            areas.push(serde_json::json!({"scope":area,"selected_files":count,"explore":explore(area,0)}));
            navigation["areas"]=serde_json::json!(&areas);
            if serde_json::to_vec(&navigation).map_err(|e|McpError::internal_error(e.to_string(),None))?.len()>24*1024 {
                areas.pop(); navigation["areas"]=serde_json::json!(&areas); break;
            }
        }
        let wanted=file_rows.len();
        let mut displayed=Vec::new();
        for mut file in file_rows {
            if req.symbol_name.is_some() {
                loop {
                    named_source_fallback(&mut file,&req.project_id);
                    navigation["files"]=serde_json::json!([&file]);
                    if serde_json::to_vec(&navigation).map_err(|e|McpError::internal_error(e.to_string(),None))?.len()<=56*1024 {break;}
                    if file["symbols"].as_array().unwrap().len()<=1 {
                        return Err(McpError::invalid_params("Named candidate metadata exceeds response bound; narrow scope/exclusions",None));
                    }
                    file["symbols"].as_array_mut().unwrap().pop();
                    let omitted=file["symbols_omitted_from_display"].as_u64().unwrap_or(0)+1;
                    file["symbols_omitted_from_display"]=serde_json::json!(omitted);
                    file["symbol_display_limited"]=serde_json::json!(true);
                }
            }
            displayed.push(file); navigation["files"]=serde_json::json!(&displayed);
            if serde_json::to_vec(&navigation).map_err(|e|McpError::internal_error(e.to_string(),None))?.len()>56*1024 {
                displayed.pop(); navigation["files"]=serde_json::json!(&displayed); break;
            }
        }
        let next=if displayed.len()<wanted{Some(req.offset+displayed.len())}else{paths.next_offset};
        if let Some(offset)=next.filter(|offset|*offset>req.offset) {
            navigation["next_page"]=match req.scope.as_deref(){Some(scope)=>explore(scope,offset),None=>
                {
                    let mut action=codemap_followup(serde_json::json!({"tool":"get_codebase_overview","arguments":{"project_id":req.project_id,
                        "exclude_paths":req.exclude_paths,"offset":offset,"limit":req.limit}}),req.path_contains.as_deref());
                    if !req.include_declarations {action["arguments"]["include_declarations"]=serde_json::json!(false);}
                    action
                }};
        }
        navigation["coverage"]["response_omitted_files"]=serde_json::json!(wanted-displayed.len());
        navigation["coverage"]["response_omitted_areas"]=serde_json::json!(paths.areas.len()-areas.len());
        if !req.include_declarations {
            // Only final displayed rows can supply the referenced literal
            // actions. An undisplayed file keeps its own area navigation.
            for (index,(area,count)) in paths.areas.iter().take(areas.len()).enumerate() {
                if *count==1 && displayed.iter().any(|file|file["path"].as_str()==Some(area.as_str())) {
                    navigation["areas"][index].as_object_mut().expect("area object").remove("explore");
                    navigation["areas"][index]["use_file_actions"]=serde_json::json!(true);
                }
            }
        }
        if req.path_contains.is_some() {
            // Allocate optional sibling browsing after existing areas, files and
            // continuations, so it cannot displace their navigation evidence.
            let mut omitted=0usize;
            for (index,(area,_)) in paths.areas.iter().take(areas.len()).enumerate() {
                if navigation["areas"][index]["use_file_actions"]==true {continue;}
                let mut browse=explore(area,0);
                browse["arguments"].as_object_mut().expect("action arguments").remove("path_contains");
                let mut trial=navigation.clone();
                trial["areas"][index]["browse_without_path_filter"]=browse;
                let explanation=serde_json::json!("explore/file/page actions preserve the filter. browse_without_path_filter explicitly clears it within that same area boundary, preserving exclusions. Directory areas can reveal neighboring selected files; a file boundary stays that file. selected_files still counts filtered matches; unfiltered counts require executing the fresh action. Follow-ups are not atomic continuations.");
                if compact_navigation && !exact_file && req.include_declarations {
                    trial["limitations"][3]=explanation;
                } else if req.include_declarations && !trial["limitations"].as_array().expect("limitations array").contains(&explanation) {
                    trial["limitations"].as_array_mut().expect("limitations array").push(explanation);
                }
                let mut area_check=trial.clone();
                area_check["files"]=serde_json::json!([]);area_check["next_page"]=serde_json::Value::Null;
                if serde_json::to_vec(&area_check).map_err(|e|McpError::internal_error(e.to_string(),None))?.len()<=24*1024
                    && serde_json::to_vec(&trial).map_err(|e|McpError::internal_error(e.to_string(),None))?.len()<=56*1024 {
                    navigation=trial;
                } else {omitted+=1;}
            }
            if omitted>0 {
                let mut trial=navigation.clone();trial["coverage"]["browse_actions_omitted"]=serde_json::json!(omitted);
                if serde_json::to_vec(&trial).map_err(|e|McpError::internal_error(e.to_string(),None))?.len()<=64*1024 {navigation=trial;}
            }
        }
        if req.include_source {
            let directory=std::path::PathBuf::from(&rec.directory);
            let project=req.project_id.clone();
            let targeted=req.symbol_node_id.is_some() || req.symbol_name.is_some();
            let source_contains=req.source_contains.clone();
            let header=format!("Codebase Overview: {}\nproject_id: {}\nproject_type: {}\nactive_generation: {}\n",rec.project_name,req.project_id,rec.project_type,generation);
            let footer=self.freshness_footer(&req.project_id,generation).await;
            let preview_hold=source_hold.clone();
            let text=tokio::task::spawn_blocking(move || {
                let _held=preview_hold;
                codemap_source_preview(navigation,&directory,&project,targeted,source_contains.as_deref(),&header,&footer)
            }).await.map_err(|e|McpError::internal_error(e.to_string(),None))?
                .map_err(|e|McpError::invalid_params(e,None))?;
            return Ok(CallToolResult::success(vec![Content::text(text)]));
        }
        let encoded=serde_json::to_string(&navigation).map_err(|e|McpError::internal_error(e.to_string(),None))?;
        if encoded.len()>64*1024{return Err(McpError::internal_error("CodeMap response metadata exceeds bound; narrow scope/exclusions",None));}
        let mut out=format!("Codebase Overview: {}\nproject_id: {}\nproject_type: {}\nactive_generation: {}\n",rec.project_name,req.project_id,rec.project_type,generation);
        if !compact_navigation {
        let summary = summary.as_ref().expect("normal overview collects its project summary");
        out.push_str(if req.path_contains.is_some() {
            "\n--- Current Source (entire held project selection; navigation scope/exclusions/path filter apply below) ---\n"
        }else{"\n--- Current Source (entire held project selection; navigation scope/exclusions apply below) ---\n"});
        match summary.source_files{Some(files)=>out.push_str(&format!("source_files: {files}\n")),
            None=>out.push_str("source_files: unknown (legacy selection has no complete file manifest)\n")};
        out.push_str(&format!("source_chunks: {}\n",summary.source_chunks));
        out.push_str("\n--- File Types (selected source files) ---\n");
        if let Some(total)=summary.source_files {
            out.push_str(&format!("denominator: {total} selected source files\n"));
            for (extension,count) in &summary.files_by_extension {
                let percent=if total>0{(*count as f64/total as f64*100.0) as u32}else{0};
                out.push_str(&format!("  {extension}: {count} ({percent}%)\n"));
            }
        }else{out.push_str("unknown (legacy selection has no complete file manifest; no file percentages calculated)\n");}
        out.push_str("\n--- Language Breakdown (selected source chunks) ---\n");
        out.push_str(&format!("denominator: {} selected source chunks (not file or byte percentages)\n",summary.source_chunks));
        for (language,count) in &summary.chunks_by_language {
            let percent=if summary.source_chunks>0{(*count as f64/summary.source_chunks as f64*100.0) as u32}else{0};
            out.push_str(&format!("  {language}: {count} ({percent}%)\n"));
        }
        }
        out.push_str("\n## CodeMap Navigation\n\n```json\n"); out.push_str(&encoded); out.push_str("\n```\n");
        if let Some((total,namespaces,insights))=retained {
            out.push_str("\n--- Retained Corpus Inventory (all namespaces and physical revisions; separate observations) ---\n");
            out.push_str(&format!("retained_index_documents: {total}\nretained_memory_documents: {}\nretained_history_documents: {}\nretained_antipattern_documents: {}\n",
                namespaces.get("memory").copied().unwrap_or(0),namespaces.get("history").copied().unwrap_or(0),namespaces.get("antipattern").copied().unwrap_or(0)));
            out.push_str("retained_vector_rows: not measured (overview does not open native vector tables)\n");
            out.push_str("\n--- Agent Memory (optional retained context; source applicability not verified by CodeMap) ---\n");
            match self.state.registry.list_memory_sections(&req.project_id) {
                Ok(mut notes)=>{
                    notes.sort_by(|a,b|b.updated_at_ms.cmp(&a.updated_at_ms));
                    let now=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                        .map_or(0,|elapsed|elapsed.as_millis().min(u64::MAX as u128) as u64);
                    let overdue=notes.iter().filter(|note|note.review_after_ms.is_some_and(|review|review<=now)).count();
                    out.push_str(&format!("memory_bank_sections: {}\nstale: {overdue} (overdue review dates only; related-file freshness uninspected)\n",notes.len()));
                    for note in notes.iter().take(8){out.push_str(&format!("  {}\n",note.title.chars().take(160).collect::<String>()));}
                }
                Err(error)=>out.push_str(&format!("memory_bank_sections: unknown; stale: unknown (metadata unavailable: {})\n",error.to_string().chars().take(256).collect::<String>())),
            }
            match insights {
                Ok((titles,observed,incomplete))=>{
                    out.push_str(&format!("insight_titles_sampled: {} (standard insight-prefix records examined: {observed}; coverage {}; recency within this bounded sample only)\n",titles.len(),if incomplete{"partial/unknown"}else{"complete standard-prefix observation"}));
                    for (title,_) in titles {out.push_str(&format!("  {title}\n"));}
                }
                Err(error)=>out.push_str(&format!("insight_titles_sampled: unknown (metadata unavailable: {})\n",error.to_string().chars().take(256).collect::<String>())),
            }
            out.push_str("Use read_memory_bank for notes and their stale-evidence warnings; retained notes/insights are not selected-source navigation facts.\n");
        }else if !compact_navigation{out.push_str("\nRetained history/memory context omitted by default; request include_retained_context=true when relevant.\n");}
        out.push_str(&self.freshness_footer(&req.project_id,generation).await);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_get_extraction_confidence(
        &self,
        req: GetExtractionConfidenceRequest,
    ) -> Result<CallToolResult, McpError> {
        let src = &req.source_content;
        let cb = req.codebehind_content.as_deref().unwrap_or("");

        let confidence = match req.extraction_type.as_str() {
            "event_wiring" => {
                let has_inherits = src.contains("Inherits=") || src.contains("Inherits \"");
                let has_codebehind =
                    !cb.is_empty() || src.contains("CodeBehind=") || src.contains("CodeFile=");
                let has_handler = cb.contains("Handles ")
                    || cb.contains("_Click")
                    || cb.contains("_Load")
                    || cb.contains("EventHandler");
                let sig_valid =
                    cb.contains("Sub ") || cb.contains("void ") || cb.contains("Function ");
                let ctrl_explicit = src.contains("ID=\"") || src.contains("id=\"");
                engram_index::score_event_wiring(
                    has_inherits,
                    has_codebehind,
                    has_handler,
                    sig_valid,
                    ctrl_explicit,
                )
            }
            "sql_trace" => {
                let has_conn = src.contains("ConnectionString")
                    || src.contains("connectionString")
                    || src.contains("SqlConnection");
                let has_param = (src.contains("@") && src.contains("Parameters.Add"))
                    || src.contains("SqlParameter")
                    || src.contains("AddWithValue");
                let table_resolved = src.contains("FROM ")
                    || src.contains("INTO ")
                    || src.contains("UPDATE ")
                    || src.contains("JOIN ");
                let col_resolved = src.contains("SELECT ") && !src.contains("SELECT *");
                let sp_verified = src.contains("CommandType.StoredProcedure")
                    || src.contains("EXEC ")
                    || src.contains("sp_");
                engram_index::score_sql_trace(
                    has_conn,
                    has_param,
                    table_resolved,
                    col_resolved,
                    sp_verified,
                )
            }
            "control_binding" => {
                let runat = src.contains("runat=\"server\"") || src.contains("runat=\"Server\"");
                let explicit_id = src.contains("ID=\"") || src.contains("id=\"");
                let designer_field = !cb.is_empty()
                    && (cb.contains("Protected WithEvents") || cb.contains("protected "));
                let cb_ref = !cb.is_empty()
                    && (cb.contains(".Text")
                        || cb.contains(".Value")
                        || cb.contains(".SelectedValue")
                        || cb.contains("FindControl"));
                engram_index::score_control_binding(runat, explicit_id, designer_field, cb_ref)
            }
            other => {
                return Err(McpError::invalid_params(
                    format!(
                        "Unknown extraction_type '{}'. Must be: event_wiring, sql_trace, control_binding",
                        other
                    ),
                    None,
                ));
            }
        };

        match confidence.band {
            engram_index::ConfidenceBand::High => {
                engram_core::metrics().extractions_high_confidence.inc();
            }
            engram_index::ConfidenceBand::Medium => {
                engram_core::metrics().extractions_medium_confidence.inc();
            }
            engram_index::ConfidenceBand::Low => {
                engram_core::metrics().extractions_low_confidence.inc();
            }
        }

        let json = serde_json::to_string_pretty(&confidence)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    pub async fn handle_map_ajax_regions(
        &self,
        req: MapAjaxRegionsRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let rec = self.ensure_project_record(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let file_path = req.file_path.clone();

        let aspx_full = safe_join(std::path::Path::new(&rec.directory), &file_path)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let aspx_content = tokio::fs::read_to_string(&aspx_full).await.map_err(|e| {
            McpError::internal_error(format!("Failed to read {}: {e}", aspx_full.display()), None)
        })?;

        let result = tokio::task::spawn_blocking(move || {
            crate::services::ajax_region_service::analyze_ajax_regions(
                &graph,
                &pid,
                &file_path,
                &aspx_content,
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // Before: the handler returned a bare `"AJAX regions for <path>"`
        // string and discarded the entire `AjaxRegionMap` struct, so
        // callers got nothing useful out of the tool. Render the full
        // structured inventory via the service's existing formatter.
        let rendered = crate::services::ajax_region_service::format_ajax_region_map(&result);
        Ok(CallToolResult::success(vec![Content::text(rendered)]))
    }

    /// Persist analyzed business logic into the searchable `business_logic`
    /// namespace. Without this write, `query_business_logic` searches an
    /// empty namespace forever — the original Phase-36 wiring rendered the
    /// analysis to the caller and dropped it on the floor.
    ///
    /// Doc identity is PATH-stable (derived from the synthetic path, not the
    /// content), so re-analyzing a changed method overwrites its previous
    /// doc instead of accumulating stale duplicates. The namespace is
    /// GlobalMutable (generation 0, no generation filter at query time).
    async fn persist_business_logic(
        &self,
        project_id: &str,
        methods: &[crate::services::business_logic_service::MethodBusinessLogic],
        complete_files: &[String],
    ) -> Result<usize, McpError> {
        use engram_core::{ContentHash, DocIdStr, RelPath};

        if methods.is_empty() && complete_files.is_empty() {
            return Ok(0);
        }
        let ps = self.ensure_project_runtime(project_id).await?;
        let namespace = engram_core::namespaces::NAMESPACE_BUSINESS_LOGIC;

        let mut docs: Vec<engram_index::IndexDoc> = Vec::with_capacity(methods.len());
        for m in methods {
            // Failed JSON must not overwrite an earlier complete analysis.
            // Its diagnostic is still returned to the requesting client.
            if !m.parse_diagnostic.is_empty()
                || (m.purpose.is_empty() && m.business_rules.is_empty())
            {
                continue;
            }
            let mut content = crate::services::business_logic_service::render_method_as_doc(m);
            content.push_str(&format!("\n_Source: {}_\n", m.file_path));

            let method_key = m
                .overload_line
                .map(|line| format!("{}__L{line}", m.method_name))
                .unwrap_or_else(|| m.method_name.clone());
            let synthetic_path = format!(
                "__business_logic/{}/{}.md",
                m.file_path.replace('\\', "/"),
                method_key
            );
            // Path-stable identity: doc_id/chunk_id derive from the path so
            // updated analyses replace (pk delete-then-add), never duplicate.
            let path_hash = ContentHash::compute(synthetic_path.as_bytes());
            let doc_id = DocIdStr::compute(&synthetic_path, 0, 0, &path_hash);
            let chunk_id = {
                let h = blake3::hash(synthetic_path.as_bytes());
                let mut b = [0u8; 8];
                b.copy_from_slice(&h.as_bytes()[..8]);
                u64::from_le_bytes(b)
            };
            let content_hash = ContentHash::compute(content.as_bytes());

            docs.push(engram_index::IndexDoc {
                generation: 0,
                chunk_id,
                path: RelPath::new(&synthetic_path),
                language: "markdown".into(),
                content,
                namespace: namespace.into(),
                author: None,
                timestamp: None,
                start_line: 0,
                end_line: 0,
                doc_id: doc_id.0,
                content_hash: content_hash.0,
            });
        }
        let n = docs.len();
        if !docs.is_empty() {
            ps.search
                .index_docs(
                    project_id,
                    &docs,
                    &tokio_util::sync::CancellationToken::new(),
                )
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        }
        // Only a successful whole-file analysis can retire old identities.
        // A single-method retry must preserve its sibling overloads.
        let prefixes: Vec<String> = complete_files
            .iter()
            .filter(|file| {
                methods.iter().filter(|m| &m.file_path == *file).all(|m| {
                    m.parse_diagnostic.is_empty()
                        && (!m.purpose.is_empty() || !m.business_rules.is_empty())
                })
            })
            .map(|file| format!("__business_logic/{}/", file.replace('\\', "/")))
            .collect();
        if !prefixes.is_empty() {
            let keep: std::collections::HashSet<String> =
                docs.iter().map(|d| d.path.as_str().to_string()).collect();
            let stale: Vec<RelPath> = ps
                .search
                .list_docs_in_namespace(project_id, namespace)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
                .into_iter()
                .filter(|d| {
                    prefixes.iter().any(|p| d.path.starts_with(p)) && !keep.contains(&d.path)
                })
                .map(|d| RelPath::new(&d.path))
                .collect();
            if !stale.is_empty() {
                ps.search
                    .delete_files(project_id, namespace, &stale)
                    .await
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            }
        }
        Ok(n)
    }

    pub async fn handle_analyze_business_logic(
        &self,
        p: crate::models::requests::AnalyzeBusinessLogicRequest,
    ) -> Result<CallToolResult, McpError> {
        let rec = self.ensure_project_record(&p.project_id).await?;
        let project_dir = rec.directory.clone();
        let dreaming = self.state.dreaming.as_ref();

        let cached_hashes: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();

        if p.line.is_some() && p.method_name.is_none() {
            return Err(McpError::invalid_params(
                "line requires method_name and file_path",
                None,
            ));
        }

        if p.method_name.is_some() && p.file_path.is_none() {
            return Ok(CallToolResult::success(vec![Content::text(
                "Error: `method_name` requires `file_path` to be specified. \
                 Provide both to analyze a specific method, or just `file_path` for all methods in a file.",
            )]));
        }

        if let Some(file_path) = &p.file_path {
            let full_path = safe_join(std::path::Path::new(&project_dir), file_path)
                .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
            let content = std::fs::read_to_string(&full_path)
                .map_err(|e| McpError::invalid_params(format!("cannot read file: {e}"), None))?;

            if let Some(method_name) = &p.method_name {
                let language =
                    crate::services::business_logic_service::detect_language(file_path, &content);
                let candidates: Vec<_> =
                    crate::services::business_logic_service::extract_logic_methods(
                        &content, language,
                    )
                    .into_iter()
                    .filter(|m| {
                        if language == "vb" {
                            m.name.eq_ignore_ascii_case(method_name)
                        } else {
                            m.name == *method_name
                        }
                    })
                    .collect();
                if candidates.len() > 1 && p.line.is_none() {
                    return Err(McpError::invalid_params(
                        format!(
                            "Ambiguous method '{method_name}'; provide line from {:?}",
                            candidates
                                .iter()
                                .map(|m| m.overload_line.unwrap_or(m.start_line))
                                .collect::<Vec<_>>()
                        ),
                        None,
                    ));
                }
                let selected = candidates.into_iter().find(|m| {
                    p.line
                        .is_none_or(|line| m.overload_line.unwrap_or(m.start_line) == line)
                });
                let Some(selected) = selected else {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Method '{method_name}' not found in {file_path}"
                    ))]));
                };

                let dependency_context = crate::services::business_outcome_dependencies::SourceContext {
                    graph: &self.state.graph, project_id: &p.project_id, root: std::path::Path::new(&project_dir),
                };
                let mut evidence = crate::services::business_outcome_dependencies::collect(
                    &selected.body, &selected.owner, language, selected.start_line, file_path, Some(&dependency_context),
                );
                evidence.return_paths = crate::services::business_return_paths::collect(file_path, &content, &selected.body, selected.start_line, language).await;
                let mut result = crate::services::business_logic_service::analyze_method_logic_with_evidence(
                    dreaming,
                    file_path,
                    &selected.name,
                    &selected.body,
                    &selected.owner,
                    language,
                    selected.start_line,
                    evidence,
                )
                .await;
                result.overload_line = selected.overload_line;

                let persisted = self
                    .persist_business_logic(&p.project_id, std::slice::from_ref(&result), &[])
                    .await?;

                let extraction_failed =
                    !result.parse_diagnostic.is_empty() || result.purpose.trim().is_empty();
                if p.output_json {
                    let json = serde_json::to_string_pretty(&result)
                        .unwrap_or_else(|e| format!("JSON error: {e}"));
                    return Ok(if extraction_failed {
                        CallToolResult::error(vec![Content::text(json)])
                    } else {
                        CallToolResult::success(vec![Content::text(json)])
                    });
                }
                let mut md = crate::services::business_logic_service::render_method_as_doc(&result);
                if extraction_failed {
                    md.push_str("\n_No business-logic documents were persisted for this failed extraction._\n");
                } else {
                    md.push_str(&format!(
                        "\n_{persisted} doc(s) persisted to the business_logic namespace ? retrieve later with query_business_logic._\n"
                    ));
                }
                return Ok(if extraction_failed {
                    CallToolResult::error(vec![Content::text(md)])
                } else {
                    CallToolResult::success(vec![Content::text(md)])
                });
            }

            // File-level mode
            let dependency_context = crate::services::business_outcome_dependencies::SourceContext {
                graph: &self.state.graph, project_id: &p.project_id, root: std::path::Path::new(&project_dir),
            };
            let (file_logic, analyzed, skipped) =
                crate::services::business_logic_service::analyze_file_logic_with_context_concurrency(
                    dreaming,
                    file_path,
                    &content,
                    &cached_hashes,
                    Some(&dependency_context),
                    p.max_concurrent,
                )
                .await;

            let persisted = self
                .persist_business_logic(
                    &p.project_id,
                    &file_logic.methods,
                    std::slice::from_ref(file_path),
                )
                .await?;

            if p.output_json {
                let json = serde_json::to_string_pretty(&file_logic)
                    .unwrap_or_else(|e| format!("JSON error: {e}"));
                return Ok(CallToolResult::success(vec![Content::text(json)]));
            }

            let mut md = format!(
                "# Business Logic — {}\n\n{}- Methods analyzed: {analyzed}\n- Cached (skipped): {skipped}\n- Persisted to business_logic namespace: {persisted}\n\n",
                file_logic.class_name, crate::services::business_logic_service::render_file_purpose(&file_logic)
            );
            for m in &file_logic.methods {
                md.push_str(&crate::services::business_logic_service::render_method_as_doc(m));
                md.push_str("---\n\n");
            }
            return Ok(CallToolResult::success(vec![Content::text(md)]));
        }

        // Full project mode
        let code_paths = crate::utils::files::discover_files_recursive(
            std::path::Path::new(&project_dir),
            &[".aspx.vb", ".aspx.cs", ".ascx.vb", ".ascx.cs", ".vb", ".cs"],
            500,
        )
        .await;

        let code_files: Vec<(String, String)> = code_paths
            .into_iter()
            .filter_map(|rel| {
                let full = safe_join(std::path::Path::new(&project_dir), &rel).ok()?;
                std::fs::read_to_string(&full).ok().map(|c| (rel, c))
            })
            .collect();

        let code_refs: Vec<(&str, &str)> = code_files
            .iter()
            .map(|(p, c)| (p.as_str(), c.as_str()))
            .collect();

        let report = crate::services::business_logic_service::analyze_project_logic(
            dreaming,
            &p.project_id,
            &code_refs,
            &cached_hashes,
            p.max_concurrent,
        )
        .await;

        let all_methods: Vec<crate::services::business_logic_service::MethodBusinessLogic> = report
            .file_summaries
            .iter()
            .flat_map(|f| f.methods.iter().cloned())
            .collect();
        let persisted = self
            .persist_business_logic(
                &p.project_id,
                &all_methods,
                &report
                    .file_summaries
                    .iter()
                    .map(|f| f.file_path.clone())
                    .collect::<Vec<_>>(),
            )
            .await?;

        if p.output_json {
            let json = serde_json::to_string_pretty(&report)
                .unwrap_or_else(|e| format!("JSON error: {e}"));
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let mut md = crate::services::business_logic_service::render_compact_markdown(&report);
        md.push_str(&format!(
            "\n_{persisted} method doc(s) persisted to the business_logic namespace — query with query_business_logic._\n"
        ));
        Ok(CallToolResult::success(vec![Content::text(md)]))
    }

    pub async fn handle_query_business_logic(
        &self,
        p: QueryBusinessLogicRequest,
    ) -> Result<CallToolResult, McpError> {
        if p.query.trim().is_empty() {
            return Err(McpError::invalid_params("query must not be blank", None));
        }
        let ps = self.ensure_project_runtime(&p.project_id).await?;
        let gen_ = self.get_active_generation(&p.project_id).await?;

        let requested_top_k = p.sanitized_top_k();
        let offset = p.sanitized_offset();
        // Freshness is a post-retrieval source check. Oversample semantic
        // candidates so legacy documents cannot occupy every requested slot
        // before current-source evidence has a chance to rank.
        let candidate_top_k = requested_top_k
            .saturating_add(offset)
            .saturating_mul(10)
            .min(crate::models::requests::MAX_SEARCH_RESULTS);
        let query = HybridQuery {
            text: p.query.clone(),
            project_id: p.project_id.clone(),
            namespace: "business_logic".to_string(),
            generation: gen_,
            top_k: candidate_top_k,
            fts_mode: "loose".to_string(),
            include_path_prefixes: None,
            exclude_path_prefixes: None,
            include_path_suffixes: None,
            language_filters: None,
            author_filter: None,
            date_after: None,
            date_before: None,
            use_mmr: false,
        };

        let hits = ps
            .search
            .search(&query, None, &tokio_util::sync::CancellationToken::new())
            .await
            .map_err(|e| {
                McpError::internal_error(format!("business-logic search failed: {e}"), None)
            })?;

        // Render the actual rules — the original implementation returned the
        // literal string "Hits: N" and threw the content away, which made
        // this tool useless to the agent regardless of extraction quality.
        if hits.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "result: no_hits in the business_logic namespace.\n\
                 hints: this namespace is only populated by analyze_business_logic — run it \
                 first (file mode for one page, project mode for everything). If analysis was \
                 already run, retry with broader domain terms.",
            )]));
        }

        let mut analyses = Vec::new();
        for h in &hits {
            let content = match ps.search.get_doc_by_doc_id(
                &p.project_id,
                engram_core::namespaces::NAMESPACE_BUSINESS_LOGIC,
                0,
                &h.doc_id,
            ) {
                Ok(Some((_, _, content, _, _))) => content,
                Ok(None) => format!(
                    "[INCOMPLETE: full stored analysis unavailable; search excerpt only]\n{}",
                    h.snippet.as_deref().unwrap_or("")
                ),
                Err(error) => format!(
                    "[INCOMPLETE: stored analysis lookup failed: {error}]\n{}",
                    h.snippet.as_deref().unwrap_or("")
                ),
            };
            analyses.push((h.doc_id.clone(), h.path.to_string(), h.score, content));
        }
        let root = ps.info.directory.clone();
        let question = p.query.clone();
        let project_id = p.project_id.clone();
        let footer = self.freshness_footer(&p.project_id, gen_).await;
        let footer = if footer.len() > 4096 {
            format!("{}\nINCOMPLETE: freshness footer truncated; call get_index_freshness.\n", super::business_source::utf8_prefix(&footer, 4000))
        } else { footer };
        let budget = 48 * 1024 - footer.len();
        let mut out = tokio::task::spawn_blocking(move || {
            super::business_source::render_matches_window(
                &project_id,
                &question,
                std::path::Path::new(&root),
                analyses,
                budget,
                offset,
                requested_top_k,
            )
        })
        .await
        .map_err(|error| {
            McpError::internal_error(format!("source verification failed: {error}"), None)
        })?;
        out.push_str(&footer);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_dream_project(
        &self,
        req: DreamProjectRequest,
    ) -> Result<CallToolResult, McpError> {
        let pid = req.project_id.clone();
        let _ = self.ensure_project_record(&pid).await?;

        let min_edge_weight = req.sanitized_min_edge_weight();
        let min_cluster_size = req.sanitized_min_cluster_size();
        let max_clusters = req.sanitized_max_clusters();

        if req.wait {
            let timeout_dur = std::time::Duration::from_secs(req.sanitized_timeout_secs());

            let result = tokio::time::timeout(
                timeout_dur,
                crate::actors::dreamer::dream_once(
                    &self.state,
                    &pid,
                    min_edge_weight,
                    min_cluster_size,
                    max_clusters,
                ),
            )
            .await;

            return match result {
                Ok(Ok(insights)) => Ok(CallToolResult::success(vec![Content::text(format!(
                    "Dream completed for project_id: {pid}\n\
                     insights_generated: {insights}\n\
                     parameters: max_clusters={max_clusters}, \
                     min_edge_weight={min_edge_weight}, \
                     min_cluster_size={min_cluster_size}"
                ))])),
                Ok(Err(e)) => Err(McpError::internal_error(e.to_string(), None)),
                Err(_) => Ok(CallToolResult::success(vec![Content::text(format!(
                    "Dream timed out after {}s for project_id: {pid}. \
                     Try increasing timeout_secs or reducing max_clusters.",
                    req.sanitized_timeout_secs()
                ))])),
            };
        }

        if let Err(e) = self.state.events_tx.send(AppEvent::TriggerDream {
            project_id: pid.clone(),
        }) {
            tracing::warn!("Failed to send TriggerDream event: {e}");
        }

        Ok(CallToolResult::success(vec![Content::text(format!(
            "🟡 Dream cycle triggered for project_id: {pid}. Use list_jobs to check status."
        ))]))
    }

    pub async fn handle_analyze_file_coding_style(
        &self,
        req: crate::models::AnalyzeFileCodingStyleRequest,
    ) -> Result<CallToolResult, McpError> {
        let ps = self.ensure_project_runtime(&req.project_id).await?;

        // MCP1: use safe_join to prevent path traversal; absolute paths and ".." rejected.
        let abs_path = safe_join(std::path::Path::new(&ps.info.directory), &req.file_path)
            .map_err(|e| McpError::invalid_request(e.to_string(), None))?;

        let resolved = self
            .state
            .paths
            .resolve_path(&abs_path)
            .map_err(|e| McpError::invalid_request(e.to_string(), None))?;

        let latest_oid = tokio::task::spawn_blocking({
            let repo_path = PathBuf::from(&ps.info.directory);
            move || -> anyhow::Result<String> {
                use engram_git::GitWalker;
                let repo = GitWalker::open_repo(&repo_path)?;
                let head = repo.head()?.peel_to_commit()?;
                Ok(head.id().to_string())
            }
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let cache_subject = if let Some(rel) =
            engram_core::RelPath::from_relative(std::path::Path::new(&ps.info.directory), &resolved)
        {
            rel.as_str().to_string()
        } else {
            req.file_path.clone()
        };
        // Versioned: a binary with a new output format must never serve a
        // previous binary's cached text as current (live, 2026-08-29).
        const STYLE_GUIDE_FORMAT: &str = "v2";
        let cache_key = format!(
            "style_guide:{STYLE_GUIDE_FORMAT}:{}:{}",
            cache_subject, latest_oid
        );

        if let Some(mut cached) = self
            .state
            .registry
            .get_meta(&req.project_id, &cache_key)
            .ok()
            .flatten()
        {
            if !cached.contains("(cached)") {
                cached.push_str("\n(cached)");
            }
            return Ok(CallToolResult::success(vec![Content::text(cached)]));
        }

        let diff_limit = req.sanitized_diff_limit();
        let result = self
            .cognitive_analyze_file_style(&req.project_id, &req.file_path, diff_limit)
            .await;

        if let Some(err) = result.error {
            return Err(McpError::internal_error(err, None));
        }

        let mut out = format!("Style Guide for {}\n\n", req.file_path);
        // The engine's computed confidence — a constant 1.00 was printed
        // here before (row-5 audit D6).
        out.push_str(&format!("Confidence: {:.2}\n", result.confidence));
        let b = &result.basis;
        out.push_str(&format!(
            "Basis: {} commit(s) (limit {}) · file read: {} · {} analyser: {} ({} rule(s)) · mimicry: {} bullet(s) · LLM: {}\n",
            b.commits,
            b.diff_limit,
            if b.file_read { "yes" } else { "NO" },
            if b.vb_analyser_ran { "VB" } else { "static" },
            if b.static_analyser_ran { "ran" } else { "skipped" },
            b.static_bullets,
            b.mimicry_bullets,
            if b.llm_used { "yes" } else { "no" }
        ));
        for f in &b.failures {
            out.push_str(&format!("FAILURE: {f}\n"));
        }
        out.push('\n');

        if let Some(guide) = result.style_guide {
            out.push_str(&guide);
        } else {
            out.push_str("No style patterns detected.");
        }

        out.push_str("\n\n---\n");
        out.push_str(&format!(
            "Analysed {} commits.",
            result.analyzed_commits.len()
        ));

        if let Err(e) = self
            .state
            .registry
            .set_meta(&req.project_id, &cache_key, &out)
        {
            tracing::warn!(project = %req.project_id, "style guide cache write failed: {e}");
            out.push_str(&format!("\n(not cached: {e})"));
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_immune_check(
        &self,
        mut req: ImmuneCheckRequest,
    ) -> Result<CallToolResult, McpError> {
        let input = crate::utils::candidate_code_input::resolve(self, &req.project_id,
            req.code.as_deref(), req.code_file.as_deref(), req.code_file_blake3.as_deref(), req.file_path.as_deref()).await?;
        let output_json = false;
        if input.evidence.is_some() { req.file_path = input.context.clone(); }
        let result = self.handle_immune_check_resolved(req, input.code).await;
        crate::utils::candidate_code_input::attach(result, input.evidence, output_json)
    }

    async fn handle_immune_check_resolved(
        &self,
        req: ImmuneCheckRequest,
        code: String,
    ) -> Result<CallToolResult, McpError> {
        if code.trim().is_empty() {
            return Err(McpError::invalid_params("code must not be empty", None));
        }
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;
        let mut evidence_gaps = Vec::new();
        match ps.search.count_docs_by_namespace(&req.project_id) {
            Ok(counts) if counts.get("antipattern").copied().unwrap_or(0) == 0 => {
                evidence_gaps.push("No anti-patterns are indexed; run analyze_reverts before treating this check as evidence.".to_string());
            }
            Err(error) => evidence_gaps.push(format!("Anti-pattern corpus availability is unknown: {error}")),
            _ => {}
        }

        let q = crate::utils::text::code_to_query(&code);
        let fts_mode = if req.use_vector { "loose" } else { "strict" };

        let hits = ps
            .search
            .search(
                &HybridQuery {
                    project_id: req.project_id.clone(),
                    namespace: "antipattern".into(),
                    generation: gen_,
                    text: q.clone(),
                    top_k: req.sanitized_top_k(),
                    fts_mode: fts_mode.into(),
                    include_path_prefixes: None,
                    exclude_path_prefixes: None,
                    include_path_suffixes: None,
                    language_filters: None,
                    author_filter: None,
                    date_after: None,
                    date_before: None,
                    use_mmr: false,
                },
                None,
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let warn_t = self
            .state
            .registry
            .get_meta(&req.project_id, "immune_warn_threshold")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<f32>().ok())
            .unwrap_or(0.6); // Default fallback

        // ── Repo-rule cross-reference ───────────────────────────────────────
        //
        // When the caller supplied a `file_path`, check which active repo
        // rules match it. `immune_*`-prefixed rules represent files that
        // were explicitly flagged (typically from a reverted commit); code
        // touching those files gets elevated scrutiny regardless of raw
        // similarity score. A CLEAN verdict on a snippet that deletes rows
        // from a previously-reverted DAL file is a false negative that
        // turns the tool into noise.
        let mut repo_rule_failure: Option<String> = None;
        let (is_immune_flagged, immune_rule_ids) = if let Some(ref fp) = req.file_path {
            let rules = match self.state.registry.list_repo_rules(&req.project_id) {
                Ok(r) => r,
                Err(e) => {
                    repo_rule_failure = Some(format!("repo rules unavailable: {e}"));
                    Vec::new()
                }
            };
            let mut matched: Vec<String> = Vec::new();
            for rule in rules {
                if !rule.rule_id.starts_with("immune_") {
                    continue;
                }
                if immune_rule_matches_path(&rule.file_pattern, fp) {
                    matched.push(rule.rule_id);
                }
            }
            (!matched.is_empty(), matched)
        } else {
            (false, Vec::new())
        };

        // ── Destructive-pattern detection ───────────────────────────────────
        //
        // Non-LLM, deterministic scan for operations that are reliably
        // dangerous on a data-access file: bulk deletes, DROP / TRUNCATE,
        // mass-mutation LINQ helpers, `ExecuteNonQuery` with a DELETE /
        // DROP / TRUNCATE literal. When the snippet matches at least one
        // of these AND the target file is immune-flagged, we force at
        // least WARN even if similarity scores are low.
        let destructive_hits = detect_destructive_patterns(&code);

        // Hybrid hit scores are reciprocal-rank-fusion values (about
        // 1/(60+rank)), not similarities, and never reach the thresholds
        // below. Similarity is the share of this code's terms found in each
        // anti-pattern's stored content.
        let mut similarities = Vec::with_capacity(hits.len());
        for hit in &hits {
            similarities.push(match ps.search.get_doc_by_pk(&hit.pk) {
                Ok(Some((_, _, content, _, _))) => crate::services::pre_commit_review_service::gates::code_similarity(&content, &q),
                Ok(None) => {
                    evidence_gaps.push(format!("anti-pattern hit {} has no backing document", hit.pk));
                    0.0
                }
                Err(e) => {
                    evidence_gaps.push(format!("anti-pattern hit {} is unreadable: {e}", hit.pk));
                    0.0
                }
            });
        }
        let match_count = hits.len();
        let highest_score = similarities.iter().copied().fold(0.0f32, f32::max);
        // Only hits similar enough count toward the match-count escalation.
        let similar_count = similarities.iter().filter(|s| **s > warn_t / 2.0).count();
        let cap = req.sanitized_top_k();
        let mut out = format!(
            "# Immune Check Result\n\n**Matches Found**: {} shown (cap top_k={}{})\n\n",
            match_count,
            cap,
            if match_count >= cap {
                " — the cap was filled; raise top_k for more"
            } else {
                ""
            }
        );
        if let Some(ref f) = repo_rule_failure {
            evidence_gaps.push(f.clone());
            out.push_str(&format!(
                "FAILURE: {f} — immune-file escalation could not run\n\n"
            ));
        }
        for (i, (hit, similarity)) in hits.iter().zip(&similarities).enumerate() {
            out.push_str(&format!(
                "### {}. {} (similarity: {:.2}; rank score {:.3})\n\n{}\n\n",
                i + 1,
                hit.path,
                similarity,
                hit.score,
                &antipattern_hit_text(hit.snippet.as_deref(), req.include_content)
            ));
        }

        // ── Verdict ladder ──────────────────────────────────────────────────
        //
        // Rank is monotonic: 0 = CLEAN, 1 = WARNING, 2 = BLOCKED. Every
        // signal proposes a floor, and the final verdict is the max of all
        // floors. That way similarity, match-count, repo-rule, and
        // destructive-pattern signals compose additively rather than
        // letting a low raw score silently override everything else.
        let mut verdict_rank: u8 = if highest_score > 0.8 {
            2
        } else if highest_score > warn_t {
            1
        } else {
            0
        };

        // Match-count escalation: 3+ matches → WARN (compounding).
        const MATCH_COUNT_WARN_THRESHOLD: usize = 3;
        let match_count_warn = similar_count >= MATCH_COUNT_WARN_THRESHOLD;
        if match_count_warn {
            verdict_rank = verdict_rank.max(1);
        }

        // Immune + any signal at all → WARN.
        let immune_any_signal =
            is_immune_flagged && (similar_count > 0 || !destructive_hits.is_empty());
        if immune_any_signal {
            verdict_rank = verdict_rank.max(1);
        }

        // Immune + destructive + a match → BLOCKED. A revert-flagged file
        // with destructive code AND anti-pattern evidence is textbook
        // "do not apply".
        if is_immune_flagged && !destructive_hits.is_empty() && similar_count > 0 {
            verdict_rank = verdict_rank.max(2);
        }

        let status = immune_status(verdict_rank, !evidence_gaps.is_empty());
        if !evidence_gaps.is_empty() {
            out.push_str("## Incomplete evidence\n\n");
            for gap in &evidence_gaps {
                out.push_str(&format!("- {gap}\n"));
            }
            out.push('\n');
        }

        // Surface the escalation reasoning so callers see WHY the verdict
        // landed where it did, not just the bare label.
        if is_immune_flagged || match_count_warn || !destructive_hits.is_empty() {
            out.push_str("## Escalation Signals\n\n");
            out.push_str(&format!(
                "- highest similarity (share of this code's terms found in an anti-pattern): {:.2} (warn above {:.2}, block above 0.80)\n",
                highest_score, warn_t
            ));
            out.push_str(&format!(
                "- similar matches (similarity above {:.2}): {} (warn at {})\n",
                warn_t / 2.0,
                similar_count,
                MATCH_COUNT_WARN_THRESHOLD
            ));
            if is_immune_flagged {
                out.push_str(&format!(
                    "- target file `{}` is immune-flagged by: {}\n",
                    req.file_path.as_deref().unwrap_or(""),
                    immune_rule_ids.join(", ")
                ));
            }
            if !destructive_hits.is_empty() {
                out.push_str(&format!(
                    "- destructive patterns detected: {}\n",
                    destructive_hits.join(", ")
                ));
            }
            out.push('\n');
        }

        out.push_str(&format!("## Final Status: {}\n", status));
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_anti_pattern_guard(
        &self,
        req: AntiPatternGuardRequest,
    ) -> Result<CallToolResult, McpError> {
        let input = crate::utils::candidate_code_input::resolve(self, &req.project_id,
            req.code.as_deref(), req.code_file.as_deref(), req.code_file_blake3.as_deref(), None).await?;
        let output_json = false;
        let result = self.handle_anti_pattern_guard_resolved(req, input.code).await;
        crate::utils::candidate_code_input::attach(result, input.evidence, output_json)
    }

    async fn handle_anti_pattern_guard_resolved(
        &self,
        req: AntiPatternGuardRequest,
        code: String,
    ) -> Result<CallToolResult, McpError> {
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;

        let count = ps.search.count_docs_by_namespace(&req.project_id)
            .map(|counts| counts.get("antipattern").copied().unwrap_or(0))
            .map_err(|error| error.to_string());
        let ap_count = match anti_pattern_corpus_preflight(count) {
            Ok(count) => count,
            Err(report) => return Ok(CallToolResult::success(vec![Content::text(report)])),
        };

        let q = crate::utils::text::code_to_query(&code);
        let fts_mode = if req.use_vector { "loose" } else { "strict" };

        let hits = ps
            .search
            .search(
                &HybridQuery {
                    project_id: req.project_id.clone(),
                    namespace: "antipattern".into(),
                    generation: gen_,
                    text: q.clone(),
                    top_k: req.sanitized_limit(),
                    fts_mode: fts_mode.into(),
                    include_path_prefixes: None,
                    exclude_path_prefixes: None,
                    include_path_suffixes: None,
                    language_filters: None,
                    author_filter: None,
                    date_after: None,
                    date_before: None,
                    use_mmr: false,
                },
                None,
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // Hit scores are reciprocal-rank-fusion values in both modes, not
        // similarities. Similarity is the share of this code's terms found in
        // each anti-pattern's stored content.
        let mut unreadable = 0usize;
        let similarities: Vec<f32> = hits
            .iter()
            .map(|h| match ps.search.get_doc_by_pk(&h.pk) {
                Ok(Some((_, _, content, _, _))) => crate::services::pre_commit_review_service::gates::code_similarity(&content, &q),
                _ => {
                    unreadable += 1;
                    0.0
                }
            })
            .collect();
        let highest_score = similarities.iter().copied().fold(0.0f32, f32::max);
        let (block_t, warn_t) = (0.85, 0.65);
        let verdict = if highest_score > block_t {
            "BLOCK"
        } else if highest_score > warn_t {
            "WARN"
        } else {
            "PASS"
        };

        let mut out = format!(
            "verdict: {}\nsimilarity: {:.2}\nindexed_patterns: {}\ncomparison: completed\nnote: Verdict applies only to matches in the queried anti-pattern index; it does not certify code safety.\n\n",
            verdict, highest_score, ap_count
        );
        if unreadable > 0 {
            out.push_str(&format!(
                "unreadable_matches: {unreadable} (backing document missing or unreadable; their similarity is unknown)\n\n"
            ));
        }
        if !hits.is_empty() {
            out.push_str("Matches in anti-pattern index:\n");
            for (h, similarity) in hits.iter().zip(&similarities).take(3) {
                out.push_str(&format!(
                    "- {} (similarity: {:.2}; rank score {:.3})\n",
                    h.path, similarity, h.score
                ));
                if req.include_content
                    && let Some(ref snippet) = h.snippet
                {
                    // Skip diff headers and show up to 5 content lines
                    let content_lines: Vec<&str> = snippet
                        .lines()
                        .filter(|l| {
                            !l.starts_with("diff ")
                                && !l.starts_with("index ")
                                && !l.starts_with("---")
                                && !l.starts_with("+++")
                                && !l.starts_with("@@")
                        })
                        .take(5)
                        .collect();
                    if !content_lines.is_empty() {
                        out.push_str(&format!("  snippet: {}\n", content_lines.join(" | ")));
                    }
                }
            }
            out.push('\n');
            out.push_str("This pattern was previously reverted as risky code.\n");
            out.push_str(
                "Consider an alternative implementation that avoids the flagged construct.\n",
            );
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_ast_dependency_graph(
        &self,
        req: AstDependencyGraphRequest,
    ) -> Result<CallToolResult, McpError> {
        let max_depth = req.sanitized_max_depth();
        let output_json = req.output_json;
        let compile_time_only = req.compile_time_only;
        let direction = req.direction; // Direction enum — no string conversion needed
        let graph = self.state.graph.clone();
        let project_id = req.project_id.clone();
        let entry_raw = req.entry.clone();

        let out = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let entry_node_id = if graph
                .get_node(&project_id, &entry_raw)
                .map_err(|e| e.to_string())?
                .is_some()
            {
                entry_raw.clone()
            } else {
                match graph
                    .resolve_symbol(&project_id, &entry_raw, None, None)
                    .map_err(|e| e.to_string())?
                {
                    ResolveResult::Unique(node) => node.node_id,
                    ResolveResult::Ambiguous(candidates) => {
                        return Err(format_ambiguous_symbol_error(&entry_raw, &candidates));
                    }
                    ResolveResult::NotFound => {
                        return Err(format!(
                            "No node found matching '{}'. Use query_graph_nodes to discover valid names/node_ids, then retry.",
                            entry_raw
                        ));
                    }
                }
            };

            let edge_kinds: Vec<EdgeKind> = if compile_time_only {
                // Calls is a compile-time (static call graph) edge kind — the
                // Roslyn path stores raw call edges as `Calls`, not
                // `Dependency`; omitting it made the compile-time traversal
                // blind to real call edges.
                vec![
                    EdgeKind::Calls,
                    EdgeKind::Dependency,
                    EdgeKind::Imports,
                    EdgeKind::Contains,
                ]
            } else {
                EdgeKind::ALL.to_vec()
            };

            // Direction enum: exhaustive match — no silent fallback possible.
            let graph_direction = direction.as_str();

            let mut traversal = graph
                .traverse(
                    &project_id,
                    &entry_node_id,
                    max_depth,
                    Some(edge_kinds),
                    graph_direction,
                )
                .map_err(|e| e.to_string())?;

            // Fallback: when the ADJ_OUT-driven traversal returns only
            // the root node, scan the raw EDGES table for any edge
            // sourced at `entry_node_id` and surface its targets as
            // depth-1 neighbours. This catches the case where an edge
            // lives in EDGES but isn't indexed into ADJ_OUT — a class
            // of bug we've seen for Contains and event_wiring edges
            // on specific node shapes — and also helps for VB LINQ
            // method chains that the extractor records as raw edges
            // without going through the full adjacency pipeline.
            //
            // Only fires for outgoing / both directions; "in" is not
            // affected because the incoming traversal uses ADJ_IN.
            let mut fallback_used = false;
            if traversal.len() <= 1 && (graph_direction == "out" || graph_direction == "both") {
                let direct_targets: Vec<engram_graph::Node> = graph
                    .list_edges(&project_id, None)
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .filter(|e| e.source_id == entry_node_id)
                    .filter_map(|e| {
                        graph
                            .get_node(&project_id, &e.target_id)
                            .ok()
                            .flatten()
                    })
                    .collect();
                if !direct_targets.is_empty() {
                    fallback_used = true;
                    for n in direct_targets {
                        // De-dup against the root in case some edge
                        // points back to the entry itself.
                        if n.node_id != entry_node_id {
                            traversal.push((n, 1));
                        }
                    }
                }
            }

            if output_json {
                let nodes_json: Vec<serde_json::Value> = traversal
                    .iter()
                    .map(|(node, depth)| {
                        serde_json::json!({
                            "node_id": node.node_id,
                            "name": node.name,
                            "node_type": node.node_type,
                            "file_path": node.file_path.as_str(),
                            "depth": depth,
                        })
                    })
                    .collect();
                serde_json::to_string_pretty(&serde_json::json!({
                    "entry_node": entry_node_id,
                    "direction": graph_direction,
                    "max_depth": max_depth,
                    "compile_time_only": compile_time_only,
                    "nodes": nodes_json,
                    "total_nodes": traversal.len(),
                    "fallback_used": fallback_used,
                }))
                .map_err(|e| e.to_string())
            } else {
                let mut tree = format!("# AST Dependency Tree: {}\n\n", entry_node_id);
                let traversal_len = traversal.len();
                for (node, depth) in traversal {
                    let indent = "  ".repeat(depth);
                    tree.push_str(&format!(
                        "{indent}- {} [{}] ({})\n",
                        node.name,
                        node.node_type,
                        node.file_path.as_str()
                    ));
                }
                if fallback_used {
                    tree.push_str(
                        "\n_Note: depth-1 neighbours below the root came from a raw-EDGES \
                         fallback scan because the ADJ_OUT-indexed traversal returned only \
                         the root. These edges exist in EDGES but aren't populated in \
                         ADJ_OUT — typically because they were written by an extractor \
                         path that doesn't maintain the adjacency index._\n",
                    );
                } else if traversal_len <= 1 {
                    tree.push_str(
                        "\n⚠️ No outgoing dependencies found. This may indicate:\n  \
                         - VB.NET LINQ method chains are not fully resolved to graph edges\n  \
                         - The symbol genuinely has no outgoing dependencies in this direction\n\
                         \n\
                         Try alternative tools:\n  \
                         - `trace_data_flow` for deeper data-access tracing\n  \
                         - `find_symbol_references` (or `direction: \"in\"` here) for \
                         incoming references\n  \
                         - `get_method_info` to inspect the symbol's body directly\n",
                    );
                }
                Ok(tree)
            }
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e: String| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_evaluate_safety(
        &self,
        req: crate::models::EvaluateSafetyRequest,
    ) -> Result<CallToolResult, McpError> {
        let eval_req = crate::services::safety_service::SafetyEvalRequest {
            project_id: req.project_id,
            affected_files: req.affected_files,
            refactor_type: req.refactor_type,
            impact_node_count: req.impact_node_count,
            impact_confidence: req.impact_confidence,
            test_coverage: req.test_coverage,
            anti_pattern_clear: req.anti_pattern_clear,
            downstream_dependents: req.downstream_dependents,
            touches_global_state: req.touches_global_state,
            touches_database: req.touches_database,
        };

        let decision = crate::services::safety_service::evaluate_safety(
            &eval_req,
            self.state.cfg.safety_policy_enabled,
            self.state.cfg.safety_min_confidence,
            self.state.cfg.safety_min_coverage,
        );

        let json = serde_json::to_string_pretty(&decision)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    pub async fn handle_dedicated_antipattern_index(
        &self,
        req: crate::models::AntipatternIndexRequest,
    ) -> Result<CallToolResult, McpError> {
        let limit = req.sanitized_limit();
        let pid = req.project_id;
        let action = req.action.to_lowercase();
        let query = req.query;
        let file_filter = req.file_filter;
        let ps = self.ensure_project_runtime(&pid).await?;
        let gen_ = self.get_active_generation(&pid).await.unwrap_or(1);

        match action.as_str() {
            "stats" => {
                let ns_counts = ps.search.count_docs_by_namespace(&pid).unwrap_or_default();
                let antipattern_docs = ns_counts.get("antipattern").copied().unwrap_or(0);

                let reg = self.state.registry.clone();
                let pid_r = pid.clone();
                let rules = tokio::task::spawn_blocking(move || reg.list_repo_rules(&pid_r))
                    .await
                    .ok()
                    .and_then(|r| r.ok())
                    .unwrap_or_default();

                let mut out = String::with_capacity(512);
                out.push_str(&format!("Anti-Pattern Index Stats: {}\n", pid));
                out.push_str(&format!("indexed_antipattern_docs: {}\n", antipattern_docs));
                out.push_str(&format!("repo_rules: {}\n", rules.len()));
                if !rules.is_empty() {
                    out.push_str("\n--- Repo Rules ---\n");
                    for r in rules.iter().take(20) {
                        out.push_str(&format!(
                            "  [{}] {} (priority={})\n",
                            r.rule_id, r.file_pattern, r.priority
                        ));
                    }
                    if rules.len() > 20 {
                        out.push_str(&format!("  ... and {} more\n", rules.len() - 20));
                    }
                }

                Ok(CallToolResult::success(vec![Content::text(
                    out.trim().to_string(),
                )]))
            }
            "list" | "search" => {
                let include_path_prefixes = file_filter.map(|f| vec![f]);
                let q_text = query.unwrap_or_else(|| "*".to_string());

                let hits = ps
                    .search
                    .search(
                        &engram_index::HybridQuery {
                            project_id: pid.clone(),
                            namespace: "antipattern".into(),
                            generation: gen_,
                            text: q_text,
                            top_k: limit,
                            fts_mode: "loose".into(),
                            include_path_prefixes,
                            exclude_path_prefixes: None,
                            include_path_suffixes: None,
                            language_filters: None,
                            author_filter: None,
                            date_after: None,
                            date_before: None,
                            use_mmr: false,
                        },
                        None,
                        &tokio_util::sync::CancellationToken::new(),
                    )
                    .await
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;

                if hits.is_empty() {
                    return Ok(CallToolResult::success(vec![Content::text(
                        "No matches found in anti-pattern index.",
                    )]));
                }

                let mut out = format!("# Anti-Pattern Matches ({})\n\n", hits.len());
                for hit in hits {
                    out.push_str(&format!("- **{}** (score: {:.3})\n", hit.path, hit.score));
                    if let Some(snippet) = hit.snippet {
                        out.push_str(&format!(
                            "  ```\n  {}\n  ```\n",
                            snippet.replace('\n', "\n  ")
                        ));
                    }
                }
                Ok(CallToolResult::success(vec![Content::text(out)]))
            }
            "clear" => {
                ps.search
                    .purge_old_generations(&pid, u64::MAX)
                    .await
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "\u{2705} Anti-pattern index cleared for project '{}'.",
                    pid
                ))]))
            }
            _ => Err(McpError::invalid_params(
                format!("Unknown action '{action}'"),
                None,
            )),
        }
    }

    pub async fn handle_compute_blast_radius(
        &self,
        req: ComputeBlastRadiusRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        if !(1..=8).contains(&req.max_depth) {
            return Err(McpError::invalid_params("max_depth must be 1..8", None));
        }
        if req.file_path.is_some() == req.symbol_fqn.is_some() {
            return Err(McpError::invalid_params("Provide exactly one of file_path or symbol_fqn", None));
        }
        if req.detail==crate::models::ImpactDetail::Summary {
            let (mut out, _source_hold, generation)=crate::services::impact_walk_service::collect_with_detail(
                &self.state,&req.project_id,req.file_path.clone(),req.symbol_fqn.clone(),req.max_depth,25,req.detail,
            ).await;
            if let Some(generation)=generation {
                out.push_str(&self.freshness_footer(&req.project_id,generation).await);
            }
            if out.len()>crate::services::impact_walk_service::SUMMARY_BYTES {
                return Err(McpError::internal_error("Impact summary exceeds response budget",None));
            }
            return Ok(CallToolResult::success(vec![Content::text(out)]));
        }
        let target_id = if let Some(ref fp) = req.file_path {
            let graph = self.state.graph.clone();
            let pid = req.project_id.clone();
            let file_node_id = format!("file:{fp}");
            let file_node_id_check = file_node_id.clone();
            let exists =
                tokio::task::spawn_blocking(move || graph.get_node(&pid, &file_node_id_check))
                    .await
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?
                    .is_some();
            if !exists {
                return Err(McpError::invalid_params(
                    format!(
                        "File path '{}' could not be resolved to '{}'. Use query_graph_nodes to locate the exact file node_id/path before retrying.",
                        fp, file_node_id
                    ),
                    None,
                ));
            }
            file_node_id
        } else if let Some(ref fqn) = req.symbol_fqn {
            let graph = self.state.graph.clone();
            let pid = req.project_id.clone();
            let fqn_c = fqn.clone();
            let found = tokio::task::spawn_blocking(move || {
                graph
                    .resolve_symbol(&pid, &fqn_c, None, None)
                    .map_err(|e| e.to_string())
            })
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e, None))?;

            match found {
                ResolveResult::Unique(node) => node.node_id,
                ResolveResult::Ambiguous(_) => {
                    // Use the same source-pinned choices as summary mode;
                    // live resolver rows alone are not admitted candidates.
                    let (mut out, _source_hold, generation) =
                        crate::services::impact_walk_service::collect_with_detail(
                            &self.state, &req.project_id, None, req.symbol_fqn.clone(),
                            req.max_depth, 25, req.detail,
                        ).await;
                    if let Some(generation) = generation {
                        out.push_str(&self.freshness_footer(&req.project_id, generation).await);
                    }
                    if out.len() > 64 * 1024 {
                        return Err(McpError::internal_error("Impact recovery exceeds response budget", None));
                    }
                    return Ok(CallToolResult::success(vec![Content::text(out)]));
                }
                ResolveResult::NotFound => {
                    return Err(McpError::invalid_params(
                        format!(
                            "Could not resolve symbol_fqn '{}'. Use query_graph_nodes to locate the exact symbol or pass a full node_id (for example sym:/file:/table:).",
                            fqn
                        ),
                        None,
                    ));
                }
            }
        } else {
            return Err(McpError::invalid_params(
                "Either file_path or symbol_fqn is required",
                None,
            ));
        };

        let (mut out, source_hold, walk_generation, advisory_scope) = crate::services::impact_walk_service::collect_for_advisory(
            &self.state, &req.project_id, None, Some(target_id.clone()), req.max_depth, 25,
        ).await;
        out.push_str(&self.historical_impact_observations(
            &req.project_id, req.file_path.clone(), req.symbol_fqn.clone(),
            source_hold.clone(), walk_generation,
        ).await);
        // The legacy score remains an explicitly separate live-graph heuristic.
        // Keep the actual selected-source capability in this worker as well as
        // the response owner, including when the request future is cancelled.
        if let Some(generation) = walk_generation {
            if req.include_guidance {
            let graph = self.state.graph.clone();
            let pid = req.project_id.clone();
            let include_guidance = req.include_guidance;
            let ranking=match self.state.peek_project_cached(&req.project_id) {
                Ok(Some(project)) => project.pagerank_cache.peek_target_score(generation,&target_id),
                Ok(None) => crate::services::pagerank_cache::TargetPageRank::unavailable("project_runtime_not_cached"),
                Err(_) => crate::services::pagerank_cache::TargetPageRank::unavailable("project_runtime_busy"),
            };
            let worker_hold = source_hold.clone();
            let advisory = tokio::task::spawn_blocking(move || {
                let _source_hold = worker_hold;
                use crate::services::impact_walk_service::AdvisoryScope;
                let members=match &advisory_scope {
                    AdvisoryScope::Symbol => None,
                    AdvisoryScope::CompleteFile(members) => Some(members.as_slice()),
                    AdvisoryScope::Unavailable(reason) => anyhow::bail!("{reason}; optional advice requires a complete admitted target component"),
                };
                crate::services::blast_radius_service::compute_request_advisory(&graph,&pid,&target_id,generation,ranking,members)
            }).await.map_err(|e| McpError::internal_error(e.to_string(), None))?;
            match advisory {
                Ok((profile,guidance)) => {
                    out.push_str("\n## Unverified one-hop advisory profile\n\n```json\n");
                    out.push_str(&serde_json::to_string(&profile).map_err(|e| McpError::internal_error(e.to_string(), None))?);
                    out.push_str("\n```\n");
                    if include_guidance && !guidance.is_empty() {
                        out.push_str("\nAdvisory guidance (not source-revision validated):\n");
                        for item in guidance.iter().take(8) {
                            let recommendation: String = item.recommendation.chars().take(512).collect();
                            out.push_str(&format!("- {}\n", recommendation));
                        }
                    }
                }
                Err(error) => out.push_str(&format!("\nOne-hop advisory unavailable: {}. The revision-admitted report above remains separate.\n", error.to_string().chars().take(512).collect::<String>())),
            }
            }
            out.push_str(&self.freshness_footer(&req.project_id, generation).await);
        } else {
            out.push_str("\nSource generation unavailable; one-hop advisory omitted rather than presented as current-source impact.\n");
        }
        // Retain through all response assembly; existing guards remain usable
        // independently of Registry lifetime.
        drop(source_hold);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    /// Generate `CLAUDE.md` + `.claude/rules/*.md` from the project's
    /// indexed graph. Language-agnostic: every section is driven by what
    /// the graph actually contains, so a Rust CLI gets language
    /// conventions + danger zones while a WebForms project gets the
    /// full treatment (state, db, auth, frontend).
    pub async fn handle_produce_claude_md(
        &self,
        req: crate::models::ProduceClaudeMdRequest,
    ) -> Result<CallToolResult, McpError> {
        use crate::services::produce_claude_md_service as svc;

        validate_project_id(&req.project_id)?;
        let rec = self.ensure_project_record(&req.project_id).await?;
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let active_gen = self.get_active_generation(&pid).await?;

        // PERF: per-section wall-clock so slow regens name their culprit.
        let mut section_clock = std::time::Instant::now();
        let mut prev_section = "setup";
        // ── 1. Language breakdown ────────────────────────────────────
        tracing::info!(
            "produce_claude_md section [{}] took {:?}",
            prev_section,
            section_clock.elapsed()
        );
        section_clock = std::time::Instant::now();
        prev_section = "1. Language breakdown";
        let lang_counts = ps.search.count_docs_by_language(&pid).unwrap_or_default();
        let total_files: usize = lang_counts.values().copied().sum();
        let mut languages: Vec<svc::LanguageShare> = lang_counts
            .iter()
            .map(|(k, v)| svc::LanguageShare {
                language: k.clone(),
                file_count: *v,
                share_percent: if total_files == 0 {
                    0.0
                } else {
                    (*v as f32 / total_files as f32) * 100.0
                },
            })
            .collect();
        languages.sort_by(|a, b| b.file_count.cmp(&a.file_count));

        // ── 2. Role description (auto from languages + framework hint) ──
        // Role: graph-derived shape + multitenant / framework
        // quirks, prepended by the README blurb when present. The
        // README blurb almost always beats whatever we synthesise
        // because it's the project maintainer's own description.
        let mut role = build_role_description(&languages, &graph, &pid);
        if let Some(blurb) = read_readme_blurb(&rec.directory) {
            role = format!("{blurb} — {role}");
        }

        // ── 3. Repo rules (immune + anti-pattern) → critical rules ────
        tracing::info!(
            "produce_claude_md section [{}] took {:?}",
            prev_section,
            section_clock.elapsed()
        );
        section_clock = std::time::Instant::now();
        prev_section = "3. Repo rules (immune + anti-pattern) → critical";
        let registry = self.state.registry.clone();
        let pid_clone = pid.clone();
        let registry_for_plan = registry.clone();
        let mut instruction_plan = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let rules = registry.list_repo_rules(&pid_clone)?;
            let mut plan = svc::instruction_plan::Plan::capture(&pid_clone, rules)?;
            plan.attach_source_references(&registry);
            Ok(plan)
        }).await.map_err(|error| McpError::internal_error(error.to_string(), None))?
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        // Authored policy bypasses text cleanup, keyword clustering and model
        // rewriting. Optional curation ranks evidence IDs only, retaining every
        // original row and disclosing omissions in the recovery ledger.
        if req.use_llm {
            let candidates = instruction_plan.curation_candidates();
            if let Some(ids) = svc::llm_curation::rank_evidence_ids(self.state.dreaming.as_ref(),
                registry_for_plan.as_ref(), &pid, svc::llm_curation::CurationInput {
                    project_context: role.clone(), max_rules: candidates.len(), candidates,
                }).await {
                instruction_plan.apply_evidence_order(&ids);
            } else {
                instruction_plan.curation_note = "LLM ranking unavailable or invalid; original deterministic evidence order retained without changing policy".into();
            }
        }
        let mut critical_rules: Vec<svc::CriticalRule> = Vec::new();

        // ── 4. Existing CLAUDE.md — merge priority ────────────────────
        let existing_md = if req.merge_existing {
            read_claude_md_if_any(&rec.directory)
        } else {
            None
        };
        if let Some(ref md) = existing_md {
            let existing_rules = svc::extract_critical_rules_from_existing(md);
            critical_rules = svc::merge_with_existing(critical_rules, existing_rules);
        }

        // ── 5a. Compute PageRank + fetch file nodes ONCE ──────────────
        tracing::info!(
            "produce_claude_md section [{}] took {:?}",
            prev_section,
            section_clock.elapsed()
        );
        section_clock = std::time::Instant::now();
        prev_section = "5a. Compute PageRank + fetch file nodes ONCE";
        //
        // PageRank is the expensive part of this tool (iterative matrix
        // op over the full graph). Both the danger-zones pass and the
        // per-language style pass need a centrality ranking; compute
        // once, pass the HashMap by reference to both.
        //
        // Same story for the file-node list: one `query_nodes` scan
        // covers every language, we filter in-memory per language.
        let graph_for_prep = graph.clone();
        let pid_for_prep = pid.clone();
        let (pagerank_map, file_nodes): (
            std::collections::HashMap<String, f32>,
            Vec<engram_graph::Node>,
        ) = tokio::task::spawn_blocking(move || {
            let pr = engram_graph::analysis::compute_pagerank(
                &graph_for_prep,
                &pid_for_prep,
                active_gen,
            )
            .ok()
            .map(|m| m.pagerank)
            .unwrap_or_default();
            let files = graph_for_prep
                .query_nodes(&pid_for_prep, Some("file"), None, None, 5000)
                .unwrap_or_default();
            (pr, files)
        })
        .await
        .unwrap_or_default();

        // ── 5b. Top-10 file nodes by PageRank → candidates for blast radius.
        let top_file_ids: Vec<String> = {
            let mut pairs: Vec<(&String, &f32)> = pagerank_map
                .iter()
                .filter(|(id, _)| id.starts_with("file:"))
                .collect();
            pairs.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap_or(std::cmp::Ordering::Equal));
            pairs
                .into_iter()
                .take(10)
                .map(|(id, _)| id.clone())
                .collect()
        };

        // ── 5c. Run all blast-radius calls in parallel. ───────────────
        tracing::info!(
            "produce_claude_md section [{}] took {:?}",
            prev_section,
            section_clock.elapsed()
        );
        section_clock = std::time::Instant::now();
        prev_section = "5c. Run all blast-radius calls in parallel.";
        //
        // Each `compute_blast_radius` does heavy work (per-kind
        // `neighbors` calls + `list_edges` scan for file targets +
        // internal PageRank). Sequentially on pilot-corpus-scale graphs
        // this was ~3s; spawn_blocking'd concurrently we bottleneck on
        // the tokio blocking threadpool instead and land around
        // ~500ms for the same 10 calls.
        let blast_handles: Vec<_> = top_file_ids
            .iter()
            .map(|file_id| {
                let g = graph.clone();
                let p = pid.clone();
                let t = file_id.clone();
                tokio::task::spawn_blocking(move || {
                    (
                        t.clone(),
                        crate::services::blast_radius_service::compute_blast_radius(
                            &g, &p, &t, active_gen, false,
                        ),
                    )
                })
            })
            .collect();

        let mut danger_zones: Vec<svc::DangerZone> = Vec::new();
        for handle in blast_handles {
            if let Ok((file_id, Ok(report))) = handle.await {
                if report.migration_risk >= 4 {
                    let path = file_id
                        .strip_prefix("file:")
                        .unwrap_or(&file_id)
                        .to_string();
                    danger_zones.push(svc::DangerZone {
                        file_path: path,
                        risk_score: report.migration_risk,
                        risk_band: report.risk_band.to_string(),
                        total_downstream: report.total_downstream,
                        reasons: reasons_from_breakdown(&report.complexity_breakdown),
                    });
                }
            }
        }
        danger_zones.sort_by(|a, b| {
            b.risk_score
                .cmp(&a.risk_score)
                .then(b.total_downstream.cmp(&a.total_downstream))
        });
        danger_zones.truncate(10);

        // ── 6. Static coding style per language (top-5% share) ────────
        tracing::info!(
            "produce_claude_md section [{}] took {:?}",
            prev_section,
            section_clock.elapsed()
        );
        section_clock = std::time::Instant::now();
        prev_section = "6. Static coding style per language (top-5% shar";
        //
        // Reuses the precomputed `pagerank_map` + `file_nodes` so the
        // per-language gather is an in-memory filter + rank, not a
        // fresh graph scan.
        let mut per_language_rules: Vec<svc::LanguageRules> = Vec::new();
        for lang in &languages {
            if lang.share_percent < 5.0 {
                continue;
            }
            let bullets = gather_language_style(
                &file_nodes,
                &pagerank_map,
                &rec.directory,
                &lang.language,
                3,
            )
            .await;
            if bullets.bullets.is_empty() {
                continue;
            }
            per_language_rules.push(svc::LanguageRules {
                language: lang.language.clone(),
                glob: svc::language_to_globs(&lang.language),
                bullets: bullets.bullets,
                sample_files: bullets.sample_files,
            });
        }

        // ── 7. Session workflow summary (only if state nodes exist) ───
        tracing::info!(
            "produce_claude_md section [{}] took {:?}",
            prev_section,
            section_clock.elapsed()
        );
        section_clock = std::time::Instant::now();
        prev_section = "7. Session workflow summary (only if state nodes";
        let state_summary = {
            let g = graph.clone();
            let p = pid.clone();
            let report = tokio::task::spawn_blocking(move || {
                crate::services::session_workflow_service::reconstruct_session_workflows(&g, &p)
            })
            .await
            .ok();
            report.filter(|r| r.total_keys > 0).map(|r| {
                use crate::services::session_workflow_service::StateScope;
                let mut session = 0;
                let mut viewstate = 0;
                let mut application = 0;
                for f in &r.workflows {
                    match f.scope {
                        StateScope::Session => session += 1,
                        StateScope::ViewState => viewstate += 1,
                        StateScope::Application => application += 1,
                        _ => {}
                    }
                }
                let mut by_fanin: Vec<(String, usize)> = r
                    .workflows
                    .iter()
                    .map(|f| (f.key.clone(), f.writers.len() + f.readers.len()))
                    .collect();
                by_fanin.sort_by(|a, b| b.1.cmp(&a.1));
                by_fanin.truncate(5);
                svc::StateSummary {
                    total_state_keys: r.total_keys,
                    session_keys: session,
                    viewstate_keys: viewstate,
                    application_keys: application,
                    cross_page_chains: r.cross_page_chains,
                    top_keys: by_fanin,
                    unresolved_accesses: 0, // filled below
                }
            })
        };
        // TODO-17: dynamic-key accesses make the inventory a lower bound —
        // count them so the generated rules say so.
        let state_summary = match state_summary {
            Some(mut s) => {
                let g = graph.clone();
                let p = pid.clone();
                let unresolved = tokio::task::spawn_blocking(move || {
                    let r = g
                        .list_edges_by_kind(
                            &p,
                            engram_graph::EdgeKind::UnresolvedStateRead,
                            usize::MAX,
                        )
                        .map(|v| v.len())
                        .unwrap_or(0);
                    let w = g
                        .list_edges_by_kind(
                            &p,
                            engram_graph::EdgeKind::UnresolvedStateWrite,
                            usize::MAX,
                        )
                        .map(|v| v.len())
                        .unwrap_or(0);
                    r + w
                })
                .await
                .unwrap_or(0);
                s.unresolved_accesses = unresolved;
                Some(s)
            }
            None => None,
        };

        // ── 8. Database summary (only if db_table nodes exist) ────────
        tracing::info!(
            "produce_claude_md section [{}] took {:?}",
            prev_section,
            section_clock.elapsed()
        );
        section_clock = std::time::Instant::now();
        prev_section = "8. Database summary (only if db_table nodes exis";
        let db_summary = {
            let g = graph.clone();
            let p = pid.clone();
            tokio::task::spawn_blocking(move || {
                let tables = g
                    .query_nodes(&p, Some("db_table"), None, None, 500)
                    .unwrap_or_default();
                if tables.is_empty() {
                    return None;
                }
                let mut with_refs: Vec<(String, usize)> = tables
                    .iter()
                    .map(|t| {
                        let refs = g
                            .find_incoming_edges_with_kind(&p, None, &t.node_id, 500)
                            .map(|v| v.len())
                            .unwrap_or(0);
                        (t.name.clone(), refs)
                    })
                    .collect();
                with_refs.sort_by(|a, b| b.1.cmp(&a.1));
                let top_tables = with_refs.iter().take(5).cloned().collect();
                Some(svc::DbSummary {
                    table_count: tables.len(),
                    top_tables,
                })
            })
            .await
            .ok()
            .flatten()
        };

        // ── 8b. CodeRabbit review_pattern nodes → per-language map ────
        tracing::info!(
            "produce_claude_md section [{}] took {:?}",
            prev_section,
            section_clock.elapsed()
        );
        section_clock = std::time::Instant::now();
        prev_section = "8b. CodeRabbit review_pattern nodes → per-langua";
        // Queries every `review_pattern` node (kind=pattern only;
        // wontFix/suppression clusters go elsewhere) and buckets them
        // by language. Metadata is the JSON blob written by
        // `code_review_ingest_service::cluster_metadata_value`.
        let coderabbit_rules_by_language = {
            let graph = self.state.graph.clone();
            let p = pid.clone();
            tokio::task::spawn_blocking(move || {
                graph.query_nodes(&p, Some("review_pattern"), None, None, 5_000)
            })
            .await
            .ok()
            .and_then(|r| r.ok())
            .map(|nodes| build_coderabbit_language_map(&nodes))
            .unwrap_or_default()
        };

        // ── 8c. GIS presence: one spatial_call edge is enough ─────────
        let has_gis = {
            let graph = self.state.graph.clone();
            let p = pid.clone();
            tokio::task::spawn_blocking(move || {
                graph.list_edges_by_kind(&p, EdgeKind::SpatialCall, 1)
            })
            .await
            .ok()
            .and_then(|r| r.ok())
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        };

        // ── 8c2. Co-change pairs from git temporal coupling. Empty until
        tracing::info!(
            "produce_claude_md section [{}] took {:?}",
            prev_section,
            section_clock.elapsed()
        );
        section_clock = std::time::Instant::now();
        prev_section = "8c2. Co-change pairs from git temporal coupling.";
        // index_git_history has run - the rules pipeline drops the section
        // when there are no pairs.
        let (co_change_pairs, cross_section_pairs) = {
            let graph = self.state.graph.clone();
            let p = pid.clone();
            tokio::task::spawn_blocking(move || {
                // Stream ALL temporal edges (1.3M on the pilot corpus) — a capped list
                // takes the first N by key order, which biases the sample
                // alphabetically, not by strength. Accumulate BOTH the top file
                // pairs AND the section-level rollup in one pass; the section
                // rollup must see the full stream (the top file pairs are
                // dominated by intra-section families like the resx set).
                let mut best = std::collections::HashMap::new();
                let mut xsec = std::collections::HashMap::new();
                if let Err(e) =
                    graph.fold_edges_by_kind(&p, engram_graph::EdgeKind::TemporalCoupling, |edge| {
                        svc::accumulate_co_change(&mut best, edge);
                        svc::accumulate_cross_section(&mut xsec, edge);
                    })
                {
                    tracing::warn!("co-change fold failed: {e:#}");
                }
                (
                    svc::finalize_co_change_pairs(best, 20),
                    svc::finalize_cross_section(xsec, 400),
                )
            })
            .await
            .unwrap_or_default()
        };

        // ── 8d. Auth summary: house guard helpers + roles from guard
        // metadata stamped on function nodes by the extractors. ────────
        let auth_summary = {
            let graph = self.state.graph.clone();
            let p = pid.clone();
            tokio::task::spawn_blocking(move || {
                let nodes = graph
                    .query_nodes(&p, Some("function"), None, None, 50_000)
                    .unwrap_or_default();
                let mut house: std::collections::HashMap<String, usize> = Default::default();
                let mut roles: std::collections::HashMap<String, usize> = Default::default();
                let mut guarded = 0usize;
                for n in &nodes {
                    let m = n.metadata.as_ref();
                    let checks = m
                        .and_then(|m| m.get("permission_checks"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if checks.is_empty() {
                        continue;
                    }
                    guarded += 1;
                    for g in checks.split(';').filter(|g| !g.is_empty()) {
                        *house.entry(g.to_lowercase()).or_default() += 1;
                    }
                    let rs = m
                        .and_then(|m| m.get("guard_roles"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    for r in rs.split(';').filter(|r| !r.is_empty()) {
                        *roles.entry(r.to_string()).or_default() += 1;
                    }
                }
                (house, roles, guarded)
            })
            .await
            .ok()
            .and_then(|(house, roles, guarded)| {
                if house.is_empty() {
                    return None;
                }
                let mut hs: Vec<_> = house.into_iter().collect();
                hs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                let mode = format!(
                    "house guard helpers: {}",
                    hs.iter()
                        .take(3)
                        .map(|(name, c)| format!("{name} ({c}x)"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                let mut rv: Vec<_> = roles.into_iter().collect();
                rv.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                Some(svc::AuthSummary {
                    mode,
                    required_roles: rv.into_iter().map(|(r, _)| r).take(10).collect(),
                    session_auth_patterns: guarded,
                })
            })
        };

        // ── 9. Build the snapshot ─────────────────────────────────────
        tracing::info!(
            "produce_claude_md section [{}] took {:?}",
            prev_section,
            section_clock.elapsed()
        );
        section_clock = std::time::Instant::now();
        prev_section = "9. Build the snapshot";
        let project_name = project_name_from_dir(&rec.directory);
        let snapshot = svc::ProjectSnapshot {
            project_name,
            role_description: role,
            languages,
            build_commands: detect_build_commands(&rec.directory),
            danger_zones,
            critical_rules,
            per_language_rules,
            state_summary,
            db_summary,
            co_change_pairs,
            cross_section_pairs,
            auth_summary,
            frontend_warnings: Vec::new(),
            existing_claude_md: existing_md.clone(),
            coderabbit_rules_by_language,
            has_gis,
            generated_from: Some((active_gen, crate::utils::ymd_utc(crate::utils::now_ms()))),
        };

        // ── 10. Render ────────────────────────────────────────────────
        tracing::info!(
            "produce_claude_md section [{}] took {:?}",
            prev_section,
            section_clock.elapsed()
        );
        section_clock = std::time::Instant::now();
        prev_section = "10. Render";
        let observed_paths = file_nodes.iter().map(|node| node.file_path.as_str()).collect::<Vec<_>>();
        instruction_plan.observe_paths(&observed_paths);
        instruction_plan.render_catalog();
        instruction_plan.mark_catalog_delivery("preview_returned");
        // Validate recovery output before any filesystem mutation.
        let _ = instruction_plan.ledger_json().map_err(|error| McpError::internal_error(error.to_string(), None))?;
        if req.write_to_disk {
            instruction_plan.validate_publication_budget().map_err(|error| McpError::internal_error(error.to_string(), None))?;
        }
        let notice = instruction_plan.root_notice();
        let (root_body, mut rule_files) = svc::render_root_and_rule_files(&snapshot, req.max_root_lines)
            .map_err(|error| McpError::internal_error(format!("Instruction scope could not be preserved: {error}"), None))?;
        let root_md = format!("{notice}{root_body}");
        if instruction_plan.has_catalog() {
            rule_files.push(svc::RuleFile { filename: "engram-repository-policy.md".into(), content: instruction_plan.catalog().into() });
        }
        let agents_md = if req.generate_agents_md {
            Some(format!("{notice}{}", svc::render_agents_md(&snapshot, req.max_root_lines.max(80))))
        } else {
            None
        };

        // ── 11. Optional disk write ───────────────────────────────────
        //
        // Safety contract for the CLAUDE.md write path:
        //
        //  1. If CLAUDE.md does NOT exist → write engram output
        //     directly. Normal case.
        //
        //  2. If CLAUDE.md exists AND the caller set
        //     `overwrite_existing: false` (default) → NEVER touch
        //     the existing file. Engram output is diverted to
        //     `CLAUDE.engram.md` so the caller can inspect it
        //     without any risk of clobbering hand-authored content.
        //
        //  3. If CLAUDE.md exists AND `overwrite_existing: true` AND
        //     `merge_existing: true` → splice the new engram block
        //     into the existing file between
        //     `<!-- engram:begin --> ... <!-- engram:end -->`
        //     markers, preserving every other byte. Back up the
        //     pre-write state to `CLAUDE.md.<unix_ts>.bak` first.
        //
        //  4. If CLAUDE.md exists AND `overwrite_existing: true` AND
        //     `merge_existing: false` → full overwrite, but still
        //     back up to `CLAUDE.md.<unix_ts>.bak` first.
        //
        // This makes the DATA-LOSS case (overwrite a hand-authored
        // CLAUDE.md without any backup) structurally impossible
        // without the caller explicitly opting into `overwrite_existing`.
        tracing::info!(
            "produce_claude_md section [{}] took {:?}",
            prev_section,
            section_clock.elapsed()
        );
        let mut written: Vec<String> = Vec::new();
        let mut ledger_delivery = "preview_returned";
        let mut notes: Vec<String> = Vec::new();
        // Always surface the rules-pipeline summary so the caller
        // sees the noise-filter / meta-clustering impact. Happens
        // regardless of whether we write to disk.
        notes.push(format!("{} original repository rules retained with identity, authority and scope; catalog uses {} / {} bytes. Overflow, learned evidence and unresolved legacy rows are returned in the complete recovery ledger. No uninstalled path is counted as delivered policy.", instruction_plan.inputs.len(), instruction_plan.catalog_bytes, instruction_plan.catalog_budget_bytes));
        notes.push(instruction_plan.curation_note.clone());
        if req.write_to_disk {
            let _guidance_lock = self.state.acquire_project_update_lock(&pid).await;
            let registry = self.state.registry.clone();
            let project = pid.clone();
            let current_rules = tokio::task::spawn_blocking(move || registry.list_repo_rules(&project)).await
                .map_err(|error| McpError::internal_error(error.to_string(), None))?
                .map_err(|error| McpError::internal_error(error.to_string(), None))?;
            instruction_plan.verify_base(&current_rules).map_err(|error| McpError::internal_error(error.to_string(), None))?;
            let mut automatic_artifacts: Vec<(String, String)> = Vec::new();
            let mut authorized_preimages = std::collections::BTreeMap::new();
            let mut dependencies = std::collections::BTreeMap::new();
            let companion_paths = rule_files.iter().map(|file| format!(".claude/rules/{}", file.filename)).collect::<Vec<_>>();
            use engram_core::safe_join;
            let project_dir = std::path::PathBuf::from(&rec.directory);
            let root_path = safe_join(&project_dir, "CLAUDE.md")
                .map_err(|e| McpError::invalid_request(e.to_string(), None))?;
            let root_exists = root_path.exists();

            // The file we will actually write to depends on safety
            // layer 1 (divert) vs layers 3/4 (overwrite).
            let (final_path, final_content) = if !root_exists {
                // Case 1: nothing there → direct write.
                (root_path.clone(), root_md.clone())
            } else if !req.overwrite_existing {
                // Case 2: divert. Never clobber a hand-authored
                // CLAUDE.md without explicit consent.
                let divert = safe_join(&project_dir, "CLAUDE.engram.md")
                    .map_err(|e| McpError::invalid_request(e.to_string(), None))?;
                crate::utils::files::ensure_git_excluded(&project_dir, "CLAUDE.engram.md");
                notes.push(
                    "Existing CLAUDE.md was left UNTOUCHED — proposed output targets \
                     CLAUDE.engram.md subject to ownership verification. Pass `overwrite_existing: true` to merge or \
                     replace it."
                        .into(),
                );
                (divert, root_md.clone())
            } else {
                // Case 3/4: explicit overwrite — always back up
                // first, then apply the requested merge mode.
                let existing = std::fs::read_to_string(&root_path)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                let backup_name = format!("CLAUDE.md.{ts}.bak");
                let backup_path = safe_join(&project_dir, &backup_name)
                    .map_err(|e| McpError::invalid_request(e.to_string(), None))?;
                crate::utils::files::ensure_git_excluded(&project_dir, "CLAUDE.md.*.bak");
                {
                    use std::io::Write as _;
                    let mut backup = std::fs::OpenOptions::new().write(true).create_new(true).open(&backup_path)
                        .map_err(|error| McpError::internal_error(error.to_string(), None))?;
                    backup.write_all(existing.as_bytes()).and_then(|_| backup.sync_all())
                        .map_err(|error| McpError::internal_error(error.to_string(), None))?;
                }
                written.push(backup_name.clone());
                notes.push(format!(
                    "Existing CLAUDE.md was backed up to `{backup_name}` before any write."
                ));
                authorized_preimages.insert("CLAUDE.md".to_owned(), blake3::hash(existing.as_bytes()).to_hex().to_string());
                let mode = req.merge_mode.as_str();
                let new_content = match mode {
                    // Full replace — user asked for it; backup still ran.
                    "replace" => root_md.clone(),
                    // Optimize rewrite — section-level classification
                    // replaces engram-owned sections with fresh output
                    // and preserves domain-specific human content.
                    "optimize" => {
                        let (rewritten, report) = svc::optimize_rewrite(&existing, &root_md);
                        notes.push(format!(
                            "Optimize rewrite: {original} → {rewritten} lines \
                             (replaced {replaced}, preserved {preserved} section(s))",
                            original = report.original_line_count,
                            rewritten = report.rewritten_line_count,
                            replaced = report.replaced_sections.len(),
                            preserved = report.preserved_sections.len(),
                        ));
                        if !report.preserved_sections.is_empty() {
                            notes.push(format!(
                                "Preserved human sections: {}",
                                report.preserved_sections.join(", ")
                            ));
                        }
                        if !report.replaced_sections.is_empty() {
                            notes.push(format!(
                                "Engram-owned sections replaced: {}",
                                report.replaced_sections.join(", ")
                            ));
                        }
                        rewritten
                    }
                    // Splice (default) — preserve everything, replace
                    // only the engram-markered block, or append if
                    // markers are absent. Conservative.
                    _ => svc::splice_engram_section(&existing, &root_md),
                };
                (root_path.clone(), new_content)
            };

            let root_relative = final_path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| "CLAUDE.md".into());
            dependencies.insert(root_relative.clone(), companion_paths.clone());
            automatic_artifacts.push((root_relative, final_content));

            // Ownership receipts, rather than filenames, govern refresh and
            // retirement of generated rules. Human files and edits survive.
            for f in &rule_files {
                automatic_artifacts.push((format!(".claude/rules/{}", f.filename), f.content.clone()));
            }
            if let Some(ref agents) = agents_md {
                let path = safe_join(&project_dir, "AGENTS.md")
                    .map_err(|e| McpError::invalid_request(e.to_string(), None))?;
                // AGENTS.md gets the same treatment — if present and
                // overwrite_existing=false, divert to AGENTS.engram.md.
                // Always back up on overwrite.
                let agents_exists = path.exists();
                let (a_path, a_content) = if !agents_exists {
                    (path.clone(), agents.clone())
                } else if !req.overwrite_existing {
                    let divert = safe_join(&project_dir, "AGENTS.engram.md")
                        .map_err(|e| McpError::invalid_request(e.to_string(), None))?;
                    notes.push(
                        "Existing AGENTS.md was left untouched — engram output diverted to \
                         AGENTS.engram.md."
                            .into(),
                    );
                    (divert, agents.clone())
                } else {
                    let existing = std::fs::read_to_string(&path)
                        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                    let ts = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0);
                    let backup_name = format!("AGENTS.md.{ts}.bak");
                    let backup_path = safe_join(&project_dir, &backup_name)
                        .map_err(|e| McpError::invalid_request(e.to_string(), None))?;
                    {
                        use std::io::Write as _;
                        let mut backup = std::fs::OpenOptions::new().write(true).create_new(true).open(&backup_path)
                            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
                        backup.write_all(existing.as_bytes()).and_then(|_| backup.sync_all())
                            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
                    }
                    written.push(backup_name);
                    authorized_preimages.insert("AGENTS.md".to_owned(), blake3::hash(existing.as_bytes()).to_hex().to_string());
                    let new_content = if req.merge_existing {
                        svc::splice_engram_section(&existing, agents)
                    } else {
                        agents.clone()
                    };
                    (path.clone(), new_content)
                };
                let relative = a_path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| "AGENTS.md".into());
                dependencies.insert(relative.clone(), companion_paths.clone());
                automatic_artifacts.push((relative, a_content));
            }
            let publication = match crate::services::generated_files::publish_bundle(
                &self.state.registry, &pid, &project_dir, "produce-claude-md", &automatic_artifacts, &dependencies, &authorized_preimages,
            ) {
                Ok(report) => report,
                Err(error) => match error.downcast::<crate::services::generated_files::PublicationFailure>() {
                    Ok(failure) => {
                        notes.push(format!("PARTIAL PUBLICATION: {failure}. Exact completed writes are listed; all proposed content remains returned for recovery."));
                        failure.report
                    }
                    Err(error) => return Err(McpError::internal_error(error.to_string(), None)),
                }
            };
            instruction_plan.mark_catalog_delivery(publication.outcomes.get(svc::instruction_plan::CATALOG_PATH).copied().unwrap_or("not_attempted"));
            instruction_plan.observed_catalog_blake3 = publication.observed_hashes.get(svc::instruction_plan::CATALOG_PATH).cloned();
            written.extend(publication.written);
            notes.extend(publication.notes);
            // Journal the delivery result as a separate owned artifact. Its
            // publication does not retire the other generator's owned files.
            let ledger = instruction_plan.ledger_json().map_err(|error| McpError::internal_error(error.to_string(), None))?;
            match crate::services::generated_files::publish(&self.state.registry, &pid, &project_dir,
                "instruction-generation-ledger", &[(svc::instruction_plan::LEDGER_PATH.into(), ledger)]) {
                Ok(report) => {
                    ledger_delivery = report.outcomes.get(svc::instruction_plan::LEDGER_PATH).copied().unwrap_or("not_attempted");
                    written.extend(report.written);
                    notes.extend(report.notes);
                }
                Err(error) => {
                    ledger_delivery = "publication_failed";
                    match error.downcast::<crate::services::generated_files::PublicationFailure>() {
                        Ok(failure) => {
                            ledger_delivery = failure.report.outcomes.get(svc::instruction_plan::LEDGER_PATH).copied().unwrap_or("not_attempted");
                            notes.push(format!("Recovery ledger publication stopped ({failure}); complete current ledger is returned below."));
                            written.extend(failure.report.written);
                            notes.extend(failure.report.notes);
                        }
                        Err(error) => notes.push(format!("Recovery ledger was not fully published ({error}); complete current ledger is returned below.")),
                    }
                }
            }

        }

        // ── 12. Build response ────────────────────────────────────────
        let mut output = String::with_capacity(8192);
        if !written.is_empty() {
            output.push_str("# Written files\n\n");
            for w in &written {
                let _ = writeln!(output, "- {w}");
            }
            output.push('\n');
        }
        if !notes.is_empty() {
            output.push_str("# Write-path notes\n\n");
            for n in &notes {
                let _ = writeln!(output, "- {n}");
            }
            output.push('\n');
        }
        let root_lines = root_md.lines().count();
        let _ = writeln!(output, "# CLAUDE.md ({root_lines} lines)\n\n{root_md}\n");
        for f in &rule_files {
            let lines = f.content.lines().count();
            let _ = writeln!(
                output,
                "# .claude/rules/{} ({} lines)\n\n{}\n",
                f.filename, lines, f.content
            );
        }
        if let Some(ref agents) = agents_md {
            let lines = agents.lines().count();
            let _ = writeln!(output, "# AGENTS.md ({lines} lines)\n\n{agents}\n");
        }

        let ledger = instruction_plan.ledger_json().map_err(|error| McpError::internal_error(error.to_string(), None))?;
        let ledger_hash = blake3::hash(ledger.as_bytes()).to_hex();
        let _ = writeln!(output, "Recovery ledger delivery: {ledger_delivery}. The hash below describes the exact returned proposal; an older/conflicting disk artifact is not asserted to contain it.\n");
        let _ = writeln!(output, "# {} (complete original rules and delivery ledger; proposed content BLAKE3={ledger_hash})\n\n```json\n{ledger}\n```\n", svc::instruction_plan::LEDGER_PATH);

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    pub async fn handle_detect_design_patterns(
        &self,
        req: DetectDesignPatternsRequest,
    ) -> Result<CallToolResult, McpError> {
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let pattern_filter = req.pattern_filter.clone();
        let limit = req.sanitized_limit();

        let pid_copy = pid.clone();
        let mut patterns = tokio::task::spawn_blocking(move || {
            crate::services::pattern_detection_service::detect_design_antipatterns(
                &graph, &pid_copy, 20, // god_threshold
                10, // spaghetti_threshold
                5,  // soup_threshold
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if !pattern_filter.is_empty() {
            patterns.retain(|p| {
                pattern_filter
                    .iter()
                    .any(|f| p.pattern_name.to_lowercase().contains(&f.to_lowercase()))
            });
        }

        patterns.truncate(limit);

        let mut out = format!(
            "# Design Pattern Analysis — {}\n\n**Total patterns detected**: {}\n\n",
            pid,
            patterns.len()
        );

        for p in patterns {
            out.push_str(&format!("### {} [{}]\n", p.pattern_name, p.severity));
            out.push_str(&format!("- **Evidence**: {}\n", p.evidence.join(", ")));
            out.push_str(&format!("- **Modern strategy**: {}\n\n", p.modern_target));
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[allow(clippy::too_many_arguments)]
    async fn evaluate_wave_decision(
        &self,
        project_id: &str,
        _default_risk_profile: &str,
        wave_items: Vec<crate::models::WaveItemInput>,
        generation: u64,
        require_runtime_evidence: bool,
        evidence_depth: &str,
        output_json: bool,
        source_view: Option<std::sync::Arc<engram_index::SourceSnapshotGuard>>,
    ) -> Result<CallToolResult, McpError> {
        use crate::services::autonomous_decision_service::{
            WaveAdpInput, evaluate_wave, format_wave_decision,
        };
        use crate::services::evidence_orchestration::{EvidenceDepth, EvidenceOverrides};

        let depth = EvidenceDepth::from_str(evidence_depth)
            .map_err(|e| McpError::invalid_params(e, None))?;
        let mut items = Vec::with_capacity(wave_items.len());

        for item in &wave_items {
            let overrides = EvidenceOverrides::default();
            let risk_profile = crate::services::autonomous_decision_service::RiskProfile::from_str(
                &item.risk_profile,
            )
            .map_err(|e| McpError::invalid_params(e, None))?;

            match crate::services::evidence_orchestration::gather_evidence_with_source_view(
                &self.state,
                project_id,
                std::slice::from_ref(&item.file_path),
                &item.change_description,
                risk_profile,
                depth,
                &overrides,
                require_runtime_evidence,
                generation,
                source_view.clone(),
            )
            .await
            {
                Ok(adp_input) => {
                    items.push((item.file_path.clone(), adp_input));
                }
                Err(e) => {
                    tracing::warn!(file = %item.file_path, error = %e, "Failed to gather evidence for wave item");
                }
            }
        }

        let wave_input = WaveAdpInput {
            wave_number: 1,
            wave_name: "Manual Wave".into(),
            items,
            cross_item_deps: 0,
        };

        let decision = evaluate_wave(&wave_input);

        if output_json {
            let json = serde_json::to_string_pretty(&decision)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        Ok(CallToolResult::success(vec![Content::text(
            format_wave_decision(&decision),
        )]))
    }

    pub async fn handle_autonomous_decision_gate(
        &self,
        req: crate::models::AutonomousDecisionGateRequest,
    ) -> Result<CallToolResult, McpError> {
        // Advertised in the schema, never read. This one gates AUTONOMOUS
        // decisions: silently dropping the evidence a caller supplied would
        // let a verdict read as evidence-backed when it was not.
        if req.runtime_evidence_batch.is_some() {
            return Err(McpError::invalid_params(
                "autonomous_decision_gate: `runtime_evidence_batch` is not implemented — the \
                 evidence would be DROPPED and the verdict would still read as \
                 evidence-backed. Use the boolean `has_runtime_evidence`, or ingest the \
                 batch first with ingest_runtime_artifacts."
                    .to_string(),
                None,
            ));
        }
        let depth = crate::services::evidence_orchestration::EvidenceDepth::from_str(&req.evidence_depth)
            .map_err(|error| McpError::invalid_params(error, None))?;
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let (gen_, source_view) = if depth == crate::services::evidence_orchestration::EvidenceDepth::Deep {
            let (generation, view) = self.pin_active_source_generation(&req.project_id, &ps.search).await?;
            (generation, Some(std::sync::Arc::new(view)))
        } else { (self.get_active_generation(&req.project_id).await?, None) };

        if let Some(wave_items) = req.wave_items {
            return self
                .evaluate_wave_decision(
                    &req.project_id,
                    &req.risk_profile,
                    wave_items,
                    gen_,
                    req.require_runtime_evidence,
                    &req.evidence_depth,
                    req.output_json,
                    source_view,
                )
                .await;
        }

        let overrides = crate::services::evidence_orchestration::EvidenceOverrides {
            extraction_confidence: req.extraction_confidence,
            extraction_type: req.extraction_type,
            trace_used_fallback: if req.trace_used_fallback {
                Some(true)
            } else {
                None
            },
            trace_candidate_count: if req.trace_candidate_count > 0 {
                Some(req.trace_candidate_count)
            } else {
                None
            },
            immune_verdict: req.immune_verdict,
            immune_confidence: req.immune_confidence,
            has_runtime_evidence: if req.has_runtime_evidence {
                Some(true)
            } else {
                None
            },
            reconciliation: None,
            safety_decision: None,
            retrieval_production_ready: None,
            retrieval_ndcg: None,
            retrieval_recall: None,
            migration_class: req.migration_class,
        };

        let risk_profile =
            crate::services::autonomous_decision_service::RiskProfile::from_str(&req.risk_profile)
                .map_err(|e| McpError::invalid_params(e, None))?;
        let adp_input = crate::services::evidence_orchestration::gather_evidence_with_source_view(
            &self.state,
            &req.project_id,
            &req.target_files,
            &req.proposed_change,
            risk_profile,
            depth,
            &overrides,
            req.require_runtime_evidence,
            gen_,
            source_view.clone(),
        )
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let raw_decision = crate::services::autonomous_decision_service::evaluate_gates(&adp_input);

        // ADP1/ADP kill-switch: apply the configured rollout policy so that the
        // deployment phase (shadow/advisory/guarded/autonomous) and the runtime
        // kill-switch are honoured.  Without this call, the raw ADP verdict is
        // returned regardless of rollout phase — bypassing the safety guardrails.
        let phase = crate::services::autonomous_decision_service::RolloutPhase::from_str(
            &self.state.cfg.adp_rollout_phase,
        )
        .map_err(|e| McpError::internal_error(format!("invalid adp_rollout_phase: {e}"), None))?;
        // ADP1: read the runtime kill-switch (OR of config + persisted registry
        // value) rather than the immutable Config field. This ensures the kill-
        // switch survives process restarts and can be toggled at runtime.
        let kill_switch = self
            .state
            .adp_kill_switch
            .load(std::sync::atomic::Ordering::Acquire);
        let decision = crate::services::autonomous_decision_service::apply_rollout_policy(
            &raw_decision,
            phase,
            kill_switch,
        );

        if req.output_json {
            let json = serde_json::to_string_pretty(&decision)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        Ok(CallToolResult::success(vec![Content::text(
            crate::services::autonomous_decision_service::format_decision(&decision),
        )]))
    }

    pub async fn handle_graph_centrality_rerank(
        &self,
        req: GraphCentralityRerankRequest,
    ) -> Result<CallToolResult, McpError> {
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let (gen_, _source_snapshot) = if req.query.is_some() && req.namespace == "memory" {
            let (generation, guard) = self.pin_active_source_generation(&req.project_id, &ps.search).await?;
            #[cfg(test)]
            crate::handlers::search_tools::selected_source_tests::after_selection(&req.project_id).await;
            (generation, Some(guard))
        } else {
            (self.get_active_generation(&req.project_id).await?, None)
        };
        let top_k = req.sanitized_top_k();
        let samples = req.sanitized_betweenness_samples();

        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let active_gen = gen_;
        let centrality: engram_graph::analysis::MultiCentrality =
            tokio::task::spawn_blocking(move || {
                engram_graph::analysis::compute_multi_centrality(&graph, &pid, active_gen, samples)
            })
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let pr_w = req.pagerank_weight;
        let deg_w = req.degree_weight;
        let bt_w = req.betweenness_weight;

        #[derive(serde::Serialize)]
        struct ScoredNode {
            node_id: String,
            blended_score: f32,
            pagerank: f32,
            in_degree: u32,
            out_degree: u32,
            betweenness: f32,
            #[serde(skip_serializing_if = "Option::is_none")]
            search_score: Option<f32>,
            #[serde(skip_serializing_if = "Option::is_none")]
            node_type: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            name: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            file_path: Option<String>,
        }

        let mut scored: Vec<ScoredNode> = Vec::new();

        if let Some(query) = &req.query {
            let hits = ps
                .search
                .search(
                    &engram_index::HybridQuery {
                        project_id: req.project_id.clone(),
                        namespace: req.namespace.clone(),
                        generation: gen_,
                        text: query.clone(),
                        top_k: top_k * 3,
                        fts_mode: "strict".to_string(),
                        include_path_prefixes: None,
                        exclude_path_prefixes: None,
                        include_path_suffixes: None,
                        language_filters: None,
                        author_filter: None,
                        date_after: None,
                        date_before: None,
                        use_mmr: false,
                    },
                    Some(&centrality.pagerank),
                    &tokio_util::sync::CancellationToken::new(),
                )
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;

            for hit in &hits {
                let file_node_id = format!("file:{}", hit.path.as_str());
                let blended = centrality.blended_score(&file_node_id, pr_w, deg_w, bt_w);
                let combined = hit.score * 0.7 + blended * 0.3;
                let (node_type, name, file_path) = if req.include_metadata {
                    (
                        Some("file".to_string()),
                        Some(hit.path.as_str().to_string()),
                        Some(hit.path.as_str().to_string()),
                    )
                } else {
                    (None, None, None)
                };
                scored.push(ScoredNode {
                    node_id: file_node_id.clone(),
                    blended_score: combined,
                    pagerank: centrality
                        .pagerank
                        .get(&file_node_id)
                        .copied()
                        .unwrap_or(0.0),
                    in_degree: centrality
                        .in_degree
                        .get(&file_node_id)
                        .copied()
                        .unwrap_or(0),
                    out_degree: centrality
                        .out_degree
                        .get(&file_node_id)
                        .copied()
                        .unwrap_or(0),
                    betweenness: centrality
                        .betweenness
                        .get(&file_node_id)
                        .copied()
                        .unwrap_or(0.0),
                    search_score: Some(hit.score),
                    node_type,
                    name,
                    file_path,
                });
            }
            scored.sort_by(|a, b| {
                b.blended_score
                    .partial_cmp(&a.blended_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        } else if let Some(node_ids) = &req.node_ids {
            let graph_store = self.state.graph.clone();
            let pid2 = req.project_id.clone();
            let node_ids_clone = node_ids.clone();
            let include_meta = req.include_metadata;
            let nodes_meta: Vec<NodeMetaTuple> = tokio::task::spawn_blocking(move || -> Vec<_> {
                let mut result = Vec::new();
                for nid in &node_ids_clone {
                    let meta = if include_meta {
                        graph_store
                            .get_node(&pid2, nid)
                            .ok()
                            .flatten()
                            .map(|n| (n.node_type, n.name, n.file_path.as_str().to_string()))
                    } else {
                        None
                    };
                    let (nt, nm, fp) = meta
                        .map(|(t, n, f)| (Some(t), Some(n), Some(f)))
                        .unwrap_or((None, None, None));
                    result.push((nid.clone(), nt, nm, fp));
                }
                result
            })
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

            for (nid, nt, nm, fp) in nodes_meta {
                let blended = centrality.blended_score(&nid, pr_w, deg_w, bt_w);
                scored.push(ScoredNode {
                    node_id: nid.clone(),
                    blended_score: blended,
                    pagerank: centrality.pagerank.get(&nid).copied().unwrap_or(0.0),
                    in_degree: centrality.in_degree.get(&nid).copied().unwrap_or(0),
                    out_degree: centrality.out_degree.get(&nid).copied().unwrap_or(0),
                    betweenness: centrality.betweenness.get(&nid).copied().unwrap_or(0.0),
                    search_score: None,
                    node_type: nt,
                    name: nm,
                    file_path: fp,
                });
            }
            scored.sort_by(|a, b| {
                b.blended_score
                    .partial_cmp(&a.blended_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        } else {
            // Mode 3: Top-N most central nodes
            let graph_store = self.state.graph.clone();
            let pid3 = req.project_id.clone();
            let include_meta = req.include_metadata;
            let mut all_scores: Vec<(String, f32)> = centrality
                .pagerank
                .keys()
                .map(|nid| {
                    let b = centrality.blended_score(nid, pr_w, deg_w, bt_w);
                    (nid.clone(), b)
                })
                .collect();
            all_scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            all_scores.truncate(top_k);
            let top_ids: Vec<String> = all_scores.iter().map(|(id, _)| id.clone()).collect();
            let nodes_meta: Vec<NodeMetaTuple> = tokio::task::spawn_blocking(move || -> Vec<_> {
                let mut result = Vec::new();
                for nid in &top_ids {
                    let meta = if include_meta {
                        graph_store
                            .get_node(&pid3, nid)
                            .ok()
                            .flatten()
                            .map(|n| (n.node_type, n.name, n.file_path.as_str().to_string()))
                    } else {
                        None
                    };
                    let (nt, nm, fp) = meta
                        .map(|(t, n, f)| (Some(t), Some(n), Some(f)))
                        .unwrap_or((None, None, None));
                    result.push((nid.clone(), nt, nm, fp));
                }
                result
            })
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

            for (nid, nt, nm, fp) in nodes_meta {
                let blended = centrality.blended_score(&nid, pr_w, deg_w, bt_w);
                scored.push(ScoredNode {
                    node_id: nid.clone(),
                    blended_score: blended,
                    pagerank: centrality.pagerank.get(&nid).copied().unwrap_or(0.0),
                    in_degree: centrality.in_degree.get(&nid).copied().unwrap_or(0),
                    out_degree: centrality.out_degree.get(&nid).copied().unwrap_or(0),
                    betweenness: centrality.betweenness.get(&nid).copied().unwrap_or(0.0),
                    search_score: None,
                    node_type: nt,
                    name: nm,
                    file_path: fp,
                });
            }
        }

        scored.truncate(top_k);

        if req.output_json {
            let json = serde_json::to_string_pretty(&scored)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let mode = if req.query.is_some() {
            "search+rerank"
        } else if req.node_ids.is_some() {
            "node scoring"
        } else {
            "top-N centrality"
        };
        let mut out = format!(
            "Graph Centrality Rerank ({mode})\nWeights: PR={pr_w:.2}, Degree={deg_w:.2}, Betweenness={bt_w:.2}\nResults: {}\n\n",
            scored.len()
        );
        for (i, node) in scored.iter().enumerate() {
            out.push_str(&format!(
                "{}. {} (blended={:.4})\n",
                i + 1,
                node.node_id,
                node.blended_score
            ));
            out.push_str(&format!(
                "   PR={:.6}  in_deg={}  out_deg={}  betw={:.4}",
                node.pagerank, node.in_degree, node.out_degree, node.betweenness
            ));
            if let Some(ss) = node.search_score {
                out.push_str(&format!("  search={ss:.4}"));
            }
            out.push('\n');
            if let Some(ref nt) = node.node_type {
                out.push_str(&format!("   type={nt}"));
            }
            if let Some(ref nm) = node.name {
                out.push_str(&format!("  name={nm}"));
            }
            if let Some(ref fp) = node.file_path {
                out.push_str(&format!("  path={fp}"));
            }
            if node.node_type.is_some() || node.name.is_some() || node.file_path.is_some() {
                out.push('\n');
            }
        }
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_benchmark_retrieval(
        &self,
        req: crate::models::BenchmarkRetrievalRequest,
    ) -> Result<CallToolResult, McpError> {
        let pid = req.project_id.clone();
        let ps = self.ensure_project_runtime(&pid).await?;
        let (generation, _source_snapshot) = self.pin_active_source_generation(&pid, &ps.search).await?;
        #[cfg(test)]
        crate::handlers::search_tools::selected_source_tests::after_selection(&pid).await;

        let queries: Vec<crate::services::benchmark_service::BenchmarkQuery> =
            if let Some(custom) = req.custom_queries {
                custom
                    .into_iter()
                    .map(|q| crate::services::benchmark_service::BenchmarkQuery {
                        query: q.query,
                        relevant_paths: q.relevant_paths,
                    })
                    .collect()
            } else {
                crate::services::benchmark_service::generate_legacy_benchmark_queries()
            };

        let mut per_query: Vec<crate::services::benchmark_service::QueryBenchmarkResult> =
            Vec::new();
        let (mut total_ndcg, mut total_recall, mut total_mrr, mut total_latency, mut max_latency) =
            (0.0f64, 0.0f64, 0.0f64, 0u64, 0u64);
        let mut latencies: Vec<u64> = Vec::new();

        for bq in &queries {
            let start = std::time::Instant::now();
            let hits = ps
                .search
                .search(
                    &engram_index::HybridQuery {
                        project_id: pid.clone(),
                        namespace: "memory".into(),
                        generation,
                        text: bq.query.clone(),
                        top_k: 10,
                        fts_mode: "strict".into(),
                        include_path_prefixes: None,
                        exclude_path_prefixes: None,
                        include_path_suffixes: None,
                        language_filters: None,
                        author_filter: None,
                        date_after: None,
                        date_before: None,
                        use_mmr: false,
                    },
                    None,
                    &tokio_util::sync::CancellationToken::new(),
                )
                .await
                .map_err(|error| McpError::internal_error(
                    format!("Retrieval benchmark search failed: {error:#}"), None,
                ))?;
            let elapsed_ms = start.elapsed().as_millis() as u64;

            let actual_paths: Vec<String> =
                hits.iter().map(|h| h.path.as_str().to_string()).collect();
            let ndcg = crate::services::benchmark_service::compute_ndcg(
                &actual_paths,
                &bq.relevant_paths,
                10,
            );
            let recall = crate::services::benchmark_service::compute_recall(
                &actual_paths,
                &bq.relevant_paths,
                10,
            );
            let mrr = crate::services::benchmark_service::compute_reciprocal_rank(
                &actual_paths,
                &bq.relevant_paths,
            );

            total_ndcg += ndcg;
            total_recall += recall;
            total_mrr += mrr;
            total_latency += elapsed_ms;
            if elapsed_ms > max_latency {
                max_latency = elapsed_ms;
            }
            latencies.push(elapsed_ms);

            per_query.push(crate::services::benchmark_service::QueryBenchmarkResult {
                query: bq.query.clone(),
                expected_top_paths: bq.relevant_paths.clone(),
                actual_top_paths: actual_paths,
                ndcg,
                recall,
                reciprocal_rank: mrr,
                latency_ms: elapsed_ms,
            });
        }

        let q_count = queries.len().max(1);
        let mean_ndcg = total_ndcg / q_count as f64;
        let mean_recall = total_recall / q_count as f64;
        let mean_mrr = total_mrr / q_count as f64;
        let mean_latency = total_latency as f64 / q_count as f64;
        latencies.sort();
        let p95_idx = ((latencies.len() as f64 * 0.95).ceil() as usize)
            .min(latencies.len())
            .saturating_sub(1);
        let p95_latency = latencies.get(p95_idx).copied().unwrap_or(0);

        let (passed_ndcg, passed_recall, production_ready) =
            crate::services::benchmark_service::evaluate_gates(
                mean_ndcg,
                mean_recall,
                self.state.cfg.retrieval_min_ndcg,
                self.state.cfg.retrieval_min_recall,
            );

        let result = crate::services::benchmark_service::BenchmarkResult {
            project_id: pid,
            timestamp_ms: now_ms(),
            query_count: queries.len(),
            ndcg_at_10: mean_ndcg,
            recall_at_10: mean_recall,
            mean_reciprocal_rank: mean_mrr,
            mean_latency_ms: mean_latency,
            p95_latency_ms: p95_latency as f64,
            passed_ndcg_gate: passed_ndcg,
            passed_recall_gate: passed_recall,
            production_ready,
            per_query_results: per_query,
        };

        if req.output_json {
            let json = serde_json::to_string_pretty(&result)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let mut out = String::with_capacity(4096);
        out.push_str(&format!(
            "Retrieval Benchmark ({} queries)\n",
            result.query_count
        ));
        out.push_str(&format!(
            "NDCG@10:  {:.3} (gate: {:.2}, {})\n",
            result.ndcg_at_10,
            self.state.cfg.retrieval_min_ndcg,
            if result.passed_ndcg_gate {
                "PASS"
            } else {
                "FAIL"
            }
        ));
        out.push_str(&format!(
            "Recall@10: {:.3} (gate: {:.2}, {})\n",
            result.recall_at_10,
            self.state.cfg.retrieval_min_recall,
            if result.passed_recall_gate {
                "PASS"
            } else {
                "FAIL"
            }
        ));
        out.push_str(&format!("MRR:      {:.3}\n", result.mean_reciprocal_rank));
        out.push_str(&format!(
            "Latency:  avg={:.0}ms p95={:.0}ms\n",
            result.mean_latency_ms, result.p95_latency_ms
        ));
        out.push_str(&format!(
            "\nProduction Ready: {}\n",
            if result.production_ready { "YES" } else { "NO" }
        ));
        for qr in &result.per_query_results {
            out.push_str(&format!(
                "\n  '{}': ndcg={:.3} recall={:.3} mrr={:.3} latency={}ms",
                qr.query, qr.ndcg, qr.recall, qr.reciprocal_rank, qr.latency_ms
            ));
        }
        Ok(CallToolResult::success(vec![Content::text(
            out.trim().to_string(),
        )]))
    }
}

// ── produce_claude_md helpers ────────────────────────────────────────────────

/// Derive a one-line role description from the language breakdown, with
/// simple framework hints based on what the graph contains. Intentionally
/// generic — no project-specific assumptions.
fn build_role_description(
    languages: &[crate::services::produce_claude_md_service::LanguageShare],
    graph: &engram_graph::GraphStore,
    project_id: &str,
) -> String {
    use crate::services::produce_claude_md_service::language_display;

    if languages.is_empty() {
        return "Indexed project.".into();
    }
    // Filter out data / markup formats before picking the top-3 —
    // "VB.NET + JavaScript + json" reads like an inventory bug.
    // JSON / XML / YAML / MD aren't programming languages; they're
    // file formats. Same for config / lockfiles.
    const NON_PROGRAMMING: &[&str] = &[
        "json",
        "xml",
        "yaml",
        "yml",
        "toml",
        "ini",
        "markdown",
        "md",
        "txt",
        "csv",
        "lock",
        "config",
        "properties",
    ];
    let is_programming = |lang: &str| -> bool {
        let lower = lang.to_ascii_lowercase();
        !NON_PROGRAMMING.iter().any(|n| *n == lower)
    };
    let names: Vec<&str> = languages
        .iter()
        .filter(|l| is_programming(&l.language))
        .take(3)
        .map(|l| language_display(&l.language))
        .collect();
    let lang_phrase = if names.is_empty() {
        // All detected languages were data formats — fall back to the
        // raw top-3 so we still say *something*.
        languages
            .iter()
            .take(3)
            .map(|l| language_display(&l.language))
            .collect::<Vec<_>>()
            .join(" + ")
    } else {
        names.join(" + ")
    };

    // Framework / data-layer / architecture hints — driven by the
    // graph's node-type inventory. What we're trying to tell the
    // agent in two sentences: "this is a <kind of app>, with <data
    // layer>, and <these architectural quirks that break generic
    // assumptions>."
    let counts = graph.count_nodes_by_type(project_id).unwrap_or_default();
    let has_pages = counts.get("page").copied().unwrap_or(0) > 0;
    let has_controls = counts.get("control").copied().unwrap_or(0) > 0;
    let has_tables = counts.get("db_table").copied().unwrap_or(0) > 0;
    let has_web_services = counts.get("web_service").copied().unwrap_or(0) > 0;
    let has_http_handlers = counts.get("http_handler").copied().unwrap_or(0) > 0;
    let has_wcf = counts.get("wcf_service").copied().unwrap_or(0) > 0;
    let has_route_handlers = counts.get("route_handler").copied().unwrap_or(0) > 0;

    // Fragment 1: architecture shape.
    let mut shape: Vec<String> = Vec::new();
    if has_pages && has_controls {
        shape.push("ASP.NET WebForms".into());
    }
    if has_route_handlers {
        shape.push("Web API".into());
    } else if has_web_services && !has_pages {
        shape.push("Web services (ASMX)".into());
    } else if has_web_services && has_pages {
        // Classic WebForms + ASMX combo like the pilot corpus.
        shape.push("ASMX + Web API".into());
    }
    if has_wcf {
        shape.push("WCF services".into());
    }
    if has_http_handlers {
        shape.push("HTTP handlers".into());
    }

    // Fragment 2: multitenant detection. Pattern: nodes whose file
    // path lives in a `multitenant/` directory, or files whose name
    // contains `multitenant` or `tenant`. Cheap — one filename-substring
    // query over existing file nodes. If the graph returned ≥3
    // distinct files in a multitenant directory, it's a real feature,
    // not a one-off.
    let multitenant_files = graph
        .query_nodes(project_id, Some("file"), None, None, 5000)
        .ok()
        .map(|nodes| {
            nodes
                .iter()
                .filter(|n| {
                    let p = n.file_path.as_str().to_ascii_lowercase();
                    p.contains("/multitenant/") || p.contains("\\multitenant\\")
                })
                .count()
        })
        .unwrap_or(0);
    let multitenant_hint = multitenant_files >= 3;

    // Fragment 3: custom TypeScript framework detection. Pattern:
    // TypeScript files in a `q/` or `Q/` directory, or references to
    // `q.ctrl.` / `q.api.` / `q.page` / `q.bind.` namespaces in code.
    // This is pilot-flavoured but generalises: any codebase with a
    // dominant internal TS namespace surfaces it here.
    let q_framework_files = graph
        .query_nodes(project_id, Some("file"), None, None, 5000)
        .ok()
        .map(|nodes| {
            nodes
                .iter()
                .filter(|n| {
                    let p = n.file_path.as_str().to_ascii_lowercase();
                    (p.ends_with(".ts") || p.ends_with(".tsx"))
                        && (p.contains("/q/") || p.contains("\\q\\"))
                })
                .count()
        })
        .unwrap_or(0);
    let custom_ts_framework = q_framework_files >= 5;

    // Fragment 4: compose. Keep it to two short sentences.
    // README blurb is prepended by the caller in the handler.
    let mut out = String::new();
    out.push_str(&lang_phrase);
    if !shape.is_empty() {
        out.push(' ');
        out.push_str(&shape.join(" + "));
    }
    if has_tables {
        out.push_str(" + SQL");
    }
    out.push('.');

    // Second sentence: quirks that break generic assumptions.
    let mut quirks: Vec<String> = Vec::new();
    if multitenant_hint {
        quirks.push("first-class multitenant mode".into());
    }
    if custom_ts_framework {
        quirks.push("custom `q` TypeScript framework".into());
    }
    if !quirks.is_empty() {
        out.push_str(" Notable: ");
        out.push_str(&quirks.join(", "));
        out.push('.');
    }

    out
}

/// Extract a 1-2 sentence blurb from the project's README if present,
/// usable as an opening line for the role description. Skips H1
/// headings, code fences, and HTML comments; returns the first
/// substantive paragraph. Cap at 280 characters — anything longer
/// bloats the root document.
fn read_readme_blurb(project_dir: &str) -> Option<String> {
    let dir = std::path::Path::new(project_dir);
    let candidates = ["README.md", "readme.md", "README.MD", "README"];
    let mut content: Option<String> = None;
    for name in &candidates {
        let full = dir.join(name);
        if let Ok(text) = std::fs::read_to_string(&full) {
            content = Some(text);
            break;
        }
    }
    let text = content?;
    // Skip H1 / blank lines / code fences; collect the first real
    // paragraph.
    let mut para = String::new();
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if trimmed.is_empty() {
            if !para.is_empty() {
                break; // paragraph end
            }
            continue;
        }
        if trimmed.starts_with('#') || trimmed.starts_with("<!--") {
            continue;
        }
        if !para.is_empty() {
            para.push(' ');
        }
        para.push_str(trimmed);
    }
    if para.is_empty() {
        return None;
    }
    // Strip trailing markdown link refs `[^1]`-style, trim quotes.
    let mut out = para.trim().to_string();
    if out.len() > 280 {
        out.truncate(280);
        while !out.is_empty() && !out.is_char_boundary(out.len()) {
            out.pop();
        }
        out.push('…');
    }
    Some(out)
}

/// Turn a [`engram_core::registry::RepoRule`] into a short rule text.
/// Prefers `rule_text` when present; falls back to a descriptive line
/// derived from the rule id / file pattern.
/// Parse a CodeRabbit repo-rule's embedded stats footer back into
/// Convert a blast-radius complexity breakdown into a short list of
/// "reasons" phrases for the danger-zones line. Only strong signals
/// (score ≥ 5.0) are surfaced.
fn reasons_from_breakdown(
    bd: &crate::services::blast_radius_service::ComplexityBreakdown,
) -> Vec<String> {
    let mut out = Vec::new();
    if bd.dependency_density_score >= 5.0 {
        out.push(format!(
            "hub ({:.0}/10 dependency)",
            bd.dependency_density_score
        ));
    }
    if bd.state_coupling_score >= 5.0 {
        out.push(format!("state ({:.0}/10)", bd.state_coupling_score));
    }
    if bd.sql_concat_score >= 5.0 {
        out.push(format!("SQL ({:.0}/10)", bd.sql_concat_score));
    }
    if bd.handles_clause_score >= 5.0 {
        out.push(format!("events ({:.0}/10)", bd.handles_clause_score));
    }
    if bd.gis_coupling_score >= 5.0 {
        out.push(format!("GIS ({:.0}/10)", bd.gis_coupling_score));
    }
    if bd.script_injection_score >= 5.0 {
        out.push(format!(
            "script-inject ({:.0}/10)",
            bd.script_injection_score
        ));
    }
    out
}

/// Turn a batch of `review_pattern` graph nodes into the per-language
/// map that `produce_claude_md` renders. Drops suppression-kind nodes
/// (wontFix clusters — those feed the antipattern gate's dampener,
/// not the agent's instructions) and skips anything whose metadata
/// blob can't be parsed.
///
/// Composite score matches `ReviewCluster::confidence`: biased toward
/// high fix_rate × PR saturation so a rule caught in 1 PR with 100%
/// fix rate still ranks below a rule caught in 5 PRs with 80% fix
/// rate. Ties broken by fix_commit presence (explicit `✅` signal).
fn build_coderabbit_language_map(
    nodes: &[engram_graph::Node],
) -> std::collections::HashMap<
    String,
    Vec<crate::services::produce_claude_md_service::CodeRabbitRule>,
> {
    use crate::services::produce_claude_md_service::CodeRabbitRule;

    let mut out: std::collections::HashMap<String, Vec<CodeRabbitRule>> =
        std::collections::HashMap::new();
    for n in nodes {
        let Some(meta) = n.metadata.as_ref() else {
            continue;
        };
        // Only positive clusters — suppression clusters aren't
        // guidance, they're anti-guidance and belong in the antipattern
        // gate's dampener path.
        let kind = meta.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        if kind != "pattern" {
            continue;
        }
        let fix_rate = meta.get("fix_rate").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
        let pr_count = meta
            .get("pr_ids")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let fix_commit = meta
            .get("fix_commit")
            .and_then(|v| v.as_str())
            .map(String::from);
        // Composite score: fix_rate is the dominant signal, PR count
        // via log₂(n+1) contributes diminishing returns, fix_commit
        // presence is a small nudge for explicit `✅ Addressed`
        // evidence.
        let pr_term = ((pr_count as f32) + 1.0).log2() / 4.0;
        let commit_term = if fix_commit.is_some() { 0.05 } else { 0.0 };
        let composite_score = (fix_rate * 0.75 + pr_term.min(0.25) + commit_term).clamp(0.0, 1.0);

        let rule = CodeRabbitRule {
            rule_text: n.name.clone(),
            fix_rate,
            pr_count,
            fix_commit,
            composite_score,
        };
        out.entry(n.language.clone()).or_default().push(rule);
    }
    // Sort each bucket by composite score desc so the renderer picks
    // the top-K deterministically.
    for list in out.values_mut() {
        list.sort_by(|a, b| {
            b.composite_score
                .partial_cmp(&a.composite_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    out
}

/// Read `CLAUDE.md` (or `claude.md`) from the project root if present.
fn read_claude_md_if_any(project_dir: &str) -> Option<String> {
    let dir = std::path::Path::new(project_dir);
    for candidate in &["CLAUDE.md", "claude.md", "CLAUDE.MD"] {
        if let Ok(full) = safe_join(dir, candidate) {
            if let Ok(text) = std::fs::read_to_string(&full) {
                return Some(text);
            }
        }
    }
    None
}

/// Inspect the project directory for well-known build/test command
/// conventions. Each entry is emitted as a single line in the
/// `<build>` block. Returns empty when nothing is detected — we never
/// guess.
fn detect_build_commands(project_dir: &str) -> Vec<String> {
    let dir = std::path::Path::new(project_dir);
    let has = |name: &str| dir.join(name).exists();
    let mut cmds = Vec::new();
    if has("Cargo.toml") {
        // Detect workspace vs single-crate and emit the right flag.
        let workspace = std::fs::read_to_string(dir.join("Cargo.toml"))
            .map(|s| s.contains("[workspace]"))
            .unwrap_or(false);
        if workspace {
            cmds.push("cargo build --release --workspace".into());
            cmds.push("cargo test --workspace".into());
        } else {
            cmds.push("cargo build --release".into());
            cmds.push("cargo test".into());
        }
        // Suggest clippy if it's configured.
        if has(".clippy.toml") || has("clippy.toml") {
            cmds.push("cargo clippy --all-targets -- -D warnings".into());
        }
    } else if has("package.json") {
        // Parse the scripts block so we emit `npm run build` /
        // `npm test` only when those scripts actually exist.
        let scripts = std::fs::read_to_string(dir.join("package.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v.get("scripts").cloned())
            .and_then(|v| v.as_object().cloned());
        cmds.push("npm install".into());
        if let Some(map) = scripts {
            for script in ["build", "test", "typecheck", "lint"] {
                if map.contains_key(script) {
                    cmds.push(format!("npm run {script}"));
                }
            }
        } else {
            cmds.push("npm test".into());
        }
    } else if has("pyproject.toml") || has("requirements.txt") {
        // Prefer `uv` when the project has a `uv.lock`.
        if has("uv.lock") {
            cmds.push("uv sync".into());
            cmds.push("uv run pytest".into());
        } else if has("poetry.lock") {
            cmds.push("poetry install".into());
            cmds.push("poetry run pytest".into());
        } else {
            cmds.push("pip install -e .".into());
            cmds.push("pytest".into());
        }
    } else if has("pom.xml") {
        cmds.push("mvn package".into());
        cmds.push("mvn test".into());
    } else if has("build.gradle") || has("build.gradle.kts") {
        let gradlew = has("gradlew") || has("gradlew.bat");
        let prefix = if gradlew { "./gradlew" } else { "gradle" };
        cmds.push(format!("{prefix} build"));
        cmds.push(format!("{prefix} test"));
    } else if has("go.mod") {
        cmds.push("go build ./...".into());
        cmds.push("go test ./...".into());
    } else if has("Makefile") || has("makefile") {
        // Scan Makefile targets and emit the first canonical one.
        let content = std::fs::read_to_string(dir.join("Makefile"))
            .or_else(|_| std::fs::read_to_string(dir.join("makefile")))
            .unwrap_or_default();
        for target in ["build", "all", "test"] {
            if content.contains(&format!("\n{target}:"))
                || content.starts_with(&format!("{target}:"))
            {
                cmds.push(format!("make {target}"));
            }
        }
        if cmds.is_empty() {
            cmds.push("make".into());
        }
    }
    // .NET — find the actual .sln (or .csproj/.vbproj) and cite it.
    // Runs AFTER the other language detectors because a project
    // might be primarily Rust/Node with a tooling .sln floating
    // around.
    if cmds.is_empty() || only_has_dotnet(dir) {
        if let Some(sln_name) = find_first_file_with_ext(dir, "sln") {
            cmds.push(format!(
                "msbuild \"{sln_name}\" /t:Build /p:Configuration=Debug"
            ));
            cmds.push(format!(
                "msbuild \"{sln_name}\" /t:Build /p:Configuration=Release"
            ));
            // Scan for a test project conventionally named
            // `<Something>.Tests.csproj` / `<Something>.Tests.vbproj`.
            if let Some(test_proj) = find_test_project(dir) {
                cmds.push(format!("vstest.console.exe \"{test_proj}\""));
            }
        } else if let Some(csproj) = find_first_file_with_ext(dir, "csproj")
            .or_else(|| find_first_file_with_ext(dir, "vbproj"))
        {
            cmds.push(format!("dotnet build \"{csproj}\""));
            cmds.push("dotnet test".into());
        }
    }
    cmds
}

/// True when the directory primarily contains .NET project files —
/// used to decide whether to also emit dotnet commands when another
/// ecosystem's files are present.
fn only_has_dotnet(dir: &std::path::Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    let mut has_dotnet = false;
    let mut has_other = false;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_ascii_lowercase();
        if name.ends_with(".sln") || name.ends_with(".csproj") || name.ends_with(".vbproj") {
            has_dotnet = true;
        } else if matches!(
            name.as_str(),
            "cargo.toml" | "package.json" | "go.mod" | "pom.xml" | "pyproject.toml"
        ) {
            has_other = true;
        }
    }
    has_dotnet && !has_other
}

fn find_first_file_with_ext(dir: &std::path::Path, ext: &str) -> Option<String> {
    std::fs::read_dir(dir).ok()?.flatten().find_map(|e| {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.to_ascii_lowercase().ends_with(&format!(".{ext}")) {
            Some(name)
        } else {
            None
        }
    })
}

fn find_test_project(dir: &std::path::Path) -> Option<String> {
    for candidate in std::fs::read_dir(dir).ok()?.flatten() {
        let path = candidate.path();
        if path.is_dir() {
            if let Some(p) = find_test_project(&path) {
                return Some(p);
            }
        } else {
            let name = candidate.file_name().to_string_lossy().into_owned();
            let lower = name.to_ascii_lowercase();
            if (lower.ends_with(".csproj") || lower.ends_with(".vbproj"))
                && (lower.contains(".tests.") || lower.contains(".test."))
            {
                return Some(
                    path.strip_prefix(dir)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    None
}

/// Derive a display-friendly project name from the project directory.
fn project_name_from_dir(project_dir: &str) -> String {
    std::path::Path::new(project_dir)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Project".into())
}

/// Result of per-language style gathering.
struct LanguageStyleBundle {
    bullets: Vec<String>,
    sample_files: Vec<String>,
}

/// Pick the top-N most central files in a given language (by
/// precomputed PageRank) and run `static_analyze_file_style` on each.
/// Dedup bullets across samples so the rule file isn't a repetitive
/// dump.
///
/// Both `file_nodes` and `pagerank_map` are precomputed once at the
/// top of the handler and shared across every language, so this
/// function performs no graph queries — just an in-memory filter +
/// rank + N file reads. File reads are issued concurrently via
/// `spawn_blocking` + `join_all` so the total time for, say, three
/// sample files is bounded by the slowest single read rather than
/// the sum.
async fn gather_language_style(
    file_nodes: &[engram_graph::Node],
    pagerank_map: &std::collections::HashMap<String, f32>,
    project_dir: &str,
    language: &str,
    sample_count: usize,
) -> LanguageStyleBundle {
    use crate::services::cognitive_service::static_analyze_file_style;

    // In-memory filter + rank. No graph round-trip.
    let mut ranked: Vec<(&engram_graph::Node, f32)> = file_nodes
        .iter()
        .filter(|n| n.language.eq_ignore_ascii_case(language))
        .map(|n| {
            let r = pagerank_map.get(&n.node_id).copied().unwrap_or(0.0);
            (n, r)
        })
        .collect();
    if ranked.is_empty() {
        return LanguageStyleBundle {
            bullets: Vec::new(),
            sample_files: Vec::new(),
        };
    }
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked.truncate(sample_count);

    // Read the sample files concurrently.
    let read_handles: Vec<_> = ranked
        .iter()
        .map(|(n, _)| {
            let path = n.file_path.as_str().to_string();
            let dir = std::path::PathBuf::from(project_dir);
            let path_for_read = path.clone();
            tokio::task::spawn_blocking(move || {
                let content = match safe_join(&dir, &path_for_read) {
                    Ok(full) => std::fs::read_to_string(full).ok(),
                    Err(_) => None,
                };
                (path, content)
            })
        })
        .collect();

    let mut all_bullets: Vec<String> = Vec::new();
    let mut sample_files: Vec<String> = Vec::new();
    for handle in read_handles {
        let Ok((path, Some(content))) = handle.await else {
            continue;
        };
        sample_files.push(path.clone());
        for b in static_analyze_file_style(&content, &path) {
            if !all_bullets.iter().any(|existing| existing == &b) {
                all_bullets.push(b);
            }
        }
    }

    LanguageStyleBundle {
        bullets: all_bullets,
        sample_files,
    }
}

#[cfg(test)]
mod tests {
    use super::{detect_destructive_patterns, immune_rule_matches_path, path_confidence_score};

    #[test]
    fn incomplete_immune_evidence_cannot_clear_or_demote_violations() {
        assert_eq!(super::immune_status(0, true), "INSUFFICIENT");
        assert!(super::immune_status(1, true).contains("WARNING"));
        assert!(super::immune_status(2, true).contains("BLOCKED"));
        assert!(super::immune_status(0, false).contains("CLEAN"));
    }

    // ── path_confidence_score ───────────────────────────────────────────

    fn node(node_type: &str) -> engram_graph::Node {
        engram_graph::Node {
            node_id: format!("{node_type}:n"),
            node_type: node_type.to_string(),
            name: "n".into(),
            namespace: "memory".into(),
            language: "vb".into(),
            file_path: engram_core::RelPath::new("p.aspx"),
            start_line: 0,
            end_line: 0,
            generation: 1,
            metadata: None,
        }
    }

    #[test]
    fn path_confidence_direct_event_wiring_scores_high() {
        // page → control → function → inline_sql — the textbook
        // direct-event path: one 0.05 Contains penalty, rest are 0.
        let path = [
            node("page"),
            node("control"),
            node("function"),
            node("inline_sql"),
        ];
        let s = path_confidence_score(&path, 0.0);
        assert!(
            (s - 0.95).abs() < 0.001,
            "direct event path should score ~0.95, got {s}"
        );
    }

    #[test]
    fn path_confidence_declarative_data_binding_scores_lower() {
        // page → control(grid) → control(datasource) → function → inline_sql.
        // Extra `control → control` hop adds a 0.25 penalty, so we're
        // at 1.00 - 0.05 (page→control) - 0.25 (control→control) - 0.00
        // (control→function) - 0.00 (→inline_sql) = 0.70.
        let path = [
            node("page"),
            node("control"),
            node("control"),
            node("function"),
            node("inline_sql"),
        ];
        let s = path_confidence_score(&path, 0.0);
        assert!(
            (s - 0.70).abs() < 0.001,
            "data-binding path should score ~0.70, got {s}"
        );
    }

    #[test]
    fn path_confidence_data_binding_lower_than_direct() {
        // The task's central claim: data-binding paths score lower than
        // direct event-wiring paths on the same trace target.
        let direct = [
            node("page"),
            node("control"),
            node("function"),
            node("inline_sql"),
        ];
        let declarative = [
            node("page"),
            node("control"),
            node("control"),
            node("function"),
            node("inline_sql"),
        ];
        assert!(
            path_confidence_score(&direct, 0.0) > path_confidence_score(&declarative, 0.0),
            "direct event path must outrank declarative-binding path"
        );
    }

    #[test]
    fn path_confidence_fallback_penalty_applies() {
        let path = [
            node("page"),
            node("control"),
            node("function"),
            node("inline_sql"),
        ];
        let with_fallback = path_confidence_score(&path, 0.2);
        let without_fallback = path_confidence_score(&path, 0.0);
        assert!(
            with_fallback < without_fallback,
            "fallback penalty must lower the score"
        );
        assert!(
            (without_fallback - with_fallback - 0.2).abs() < 0.001,
            "fallback penalty must subtract exactly the requested amount"
        );
    }

    #[test]
    fn path_confidence_floored_at_ten_percent() {
        // Degenerate long chain of unknown hops — score should floor
        // at 0.10 rather than going negative.
        let path = vec![node("other"); 20];
        let s = path_confidence_score(&path, 0.8);
        assert!(
            (s - 0.10).abs() < 0.001,
            "score must floor at 0.10, got {s}"
        );
    }

    #[test]
    fn path_confidence_trivial_path_is_full() {
        let path = [node("control")];
        assert!((path_confidence_score(&path, 0.0) - 1.0).abs() < 0.001);
    }

    // ── immune_rule_matches_path ─────────────────────────────────────────

    #[test]
    fn immune_rule_matches_exact_path() {
        assert!(immune_rule_matches_path(
            "Site/App_Code/arbetsorder.vb",
            "Site/App_Code/arbetsorder.vb"
        ));
    }

    #[test]
    fn immune_rule_matches_path_ignores_slash_direction() {
        // The rule might have been stored with Windows backslashes but the
        // target path carries forward slashes (or vice versa).
        assert!(immune_rule_matches_path(
            "Site\\App_Code\\arbetsorder.vb",
            "Site/App_Code/arbetsorder.vb"
        ));
    }

    #[test]
    fn immune_rule_matches_path_is_case_insensitive() {
        assert!(immune_rule_matches_path(
            "site/app_code/arbetsorder.vb",
            "Site/App_Code/ArbetsOrder.vb"
        ));
    }

    #[test]
    fn immune_rule_matches_glob_star() {
        assert!(immune_rule_matches_path(
            "Site/App_Code/*.vb",
            "Site/App_Code/arbetsorder.vb"
        ));
        assert!(immune_rule_matches_path(
            "**/arbetsorder.vb",
            "Site/App_Code/arbetsorder.vb"
        ));
        assert!(!immune_rule_matches_path(
            "Site/App_Code/*.cs",
            "Site/App_Code/arbetsorder.vb"
        ));
    }

    #[test]
    fn immune_rule_matches_plain_substring_without_globs() {
        // Bare patterns without metacharacters fall back to substring match
        // so a rule keyed on `arbetsorder.vb` catches the file regardless of
        // which directory the caller passes.
        assert!(immune_rule_matches_path(
            "arbetsorder.vb",
            "Site/App_Code/arbetsorder.vb"
        ));
    }

    #[test]
    fn immune_rule_empty_pattern_never_matches() {
        assert!(!immune_rule_matches_path("", "anything.vb"));
    }

    // ── detect_destructive_patterns ──────────────────────────────────────

    #[test]
    fn detect_destructive_flags_linq_bulk_delete() {
        let hits = detect_destructive_patterns(
            "db.ao_arbetsorder.DeleteAllOnSubmit(db.ao_arbetsorder.Where(x => x.active))",
        );
        assert!(hits.iter().any(|h| h == "DeleteAllOnSubmit"));
    }

    #[test]
    fn detect_destructive_flags_drop_and_truncate() {
        let hits = detect_destructive_patterns(
            "BEGIN TRANSACTION\nDROP TABLE audit_log\nTRUNCATE TABLE staging\nCOMMIT",
        );
        assert!(hits.iter().any(|h| h == "DROP TABLE"));
        assert!(hits.iter().any(|h| h == "TRUNCATE TABLE"));
    }

    #[test]
    fn detect_destructive_flags_execute_nonquery_with_delete() {
        let hits = detect_destructive_patterns(
            "var cmd = new SqlCommand(\"DELETE FROM orders\", conn); cmd.ExecuteNonQuery();",
        );
        // At least one destructive signal must fire — the regex family
        // catches either the unbounded DELETE or the ExecuteNonQuery+DELETE
        // combo, depending on spacing.
        assert!(
            !hits.is_empty(),
            "destructive regex family must match ExecuteNonQuery with DELETE literal, got: {hits:?}"
        );
    }

    #[test]
    fn detect_destructive_ignores_benign_snippets() {
        let hits = detect_destructive_patterns(
            "Public Function GetUser(id As Integer) As User\n  Return db.Users.FirstOrDefault(Function(u) u.Id = id)\nEnd Function",
        );
        assert!(
            hits.is_empty(),
            "benign read-only snippet must not trigger destructive flags, got: {hits:?}"
        );
    }

    #[test]
    fn detect_destructive_does_not_false_fire_on_identifier_substrings() {
        // `DROPPED_COLUMN_NAME` used as an identifier must not fire the
        // `DROP TABLE` pattern — `\b` boundaries guard against this.
        let hits = detect_destructive_patterns("Dim DROPPED_COLUMN_NAME As String = \"a\"");
        assert!(
            !hits.iter().any(|h| h == "DROP TABLE"),
            "DROPPED_COLUMN_NAME identifier must not trigger DROP TABLE, got: {hits:?}"
        );
    }
}

/// The graph's edge-kind histogram, EVERY kind listed, largest first.
///
/// External audit 2026-08-29: the overview used to print the 15 most frequent
/// kinds and hide the rest behind "... and N more kinds" — a live graph with
/// 180 fresh `co_occurrence` edges read as having none. A histogram that hides
/// kinds is not a histogram; the ~30 kinds fit on one screen.
pub fn render_edge_kind_histogram(counts: &std::collections::HashMap<String, usize>) -> String {
    if counts.is_empty() {
        return String::new();
    }
    let mut eks: Vec<(&String, &usize)> = counts.iter().collect();
    eks.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let total_edges: usize = counts.values().sum();
    let mut out = format!(
        "\n--- Edge Types ({} total, {} kinds) ---\n",
        total_edges,
        eks.len()
    );
    for (ekind, count) in eks {
        out.push_str(&format!("  {}: {}\n", ekind, count));
    }
    out
}

/// Keep missing evidence distinct from a completed search with no risky match.
fn anti_pattern_corpus_preflight(count: Result<usize, String>) -> Result<usize, String> {
    match count {
        Ok(0) => Err("verdict: INSUFFICIENT\nindexed_patterns: 0\ncomparison: not_run\nnote: No anti-patterns indexed for this project; pattern evidence must be populated before this check can run.".into()),
        Ok(count) => Ok(count),
        Err(error) => Err(format!("verdict: UNAVAILABLE\ncomparison: not_run\nnote: Could not read anti-pattern index counts: {error}")),
    }
}

#[cfg(test)]
mod anti_pattern_evidence_tests {
    use super::anti_pattern_corpus_preflight;

    #[test]
    fn missing_and_unavailable_evidence_never_pass() {
        let empty = anti_pattern_corpus_preflight(Ok(0)).unwrap_err();
        assert!(empty.starts_with("verdict: INSUFFICIENT\n"));
        assert!(empty.contains("comparison: not_run"));
        let failed = anti_pattern_corpus_preflight(Err("synthetic provider read failure".into())).unwrap_err();
        assert!(failed.starts_with("verdict: UNAVAILABLE\n"));
        assert!(failed.contains("synthetic provider read failure"));
        assert!(failed.contains("comparison: not_run"));
        assert_eq!(anti_pattern_corpus_preflight(Ok(2)), Ok(2));
    }
}
