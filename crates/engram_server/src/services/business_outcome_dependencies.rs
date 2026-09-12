//! Bounded source evidence for calls immediately preceding a return.
//! Evidence presence never establishes whether a helper returns or throws.
use engram_core::{ContentHash, safe_join};
use engram_graph::GraphStore;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{path::Path, sync::LazyLock};

pub const VERSION: &str = "immediate-return-dependencies-v1";
const EXECUTION_STAGE_CONTRACT: &str = "execution-stage-v2";
const MAX_DEPENDENCIES: usize = 32;
const MAX_HELPERS: usize = 4;
const MAX_HELPER_BYTES: usize = 8192;
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OutcomeEvidence {
    pub version: String,
    /// Versioned supporting instructions, independent of the member extraction template.
    /// Absent in legacy analyses; included in the analysis fingerprint when supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_stage_contract: Option<String>,
    pub scope: String,
    #[serde(default)]
    pub caller_file: String,
    #[serde(default)]
    pub caller_owner: String,
    #[serde(default)]
    pub caller_start_line: u32,
    #[serde(default)]
    pub language: String,
    pub dependencies: Vec<OutcomeDependency>,
    pub omissions: Vec<String>,
    pub analysis_fingerprint: String,
    #[serde(default)]
    pub reaching_context: Option<super::business_reaching_context::ReachingContext>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_paths: Option<super::business_return_paths::ReturnPathEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutcomeDependency {
    pub callee_expression: String,
    pub call_line: u32,
    pub return_line: u32,
    /// Conservative syntactic guard region, not a control-flow proof.
    pub region_start_line: u32,
    pub evidence_status: String,
    pub outcome_status: String,
    pub reason: String,
    pub helper_identity: Option<String>,
    pub helper_file: Option<String>,
    pub helper_start_line: Option<u32>,
    pub helper_end_line: Option<u32>,
    pub helper_file_hash: Option<String>,
    pub helper_body_hash: Option<String>,
    #[serde(skip)]
    pub helper_body: Option<String>,
}

pub struct SourceContext<'a> {
    pub graph: &'a GraphStore,
    pub project_id: &'a str,
    pub root: &'a Path,
}

/// Preserve newlines while masking comments/literals across line boundaries.
pub(crate) fn executable_lines(source: &str, vb: bool) -> String {
    mask_source_text(source, vb, true)
}

/// Mask comments but retain literal bytes for explicit SQL/config-key references.
/// Text presence remains lexical evidence, not executable use or symbol binding.
pub(crate) fn source_without_comments(source: &str, vb: bool) -> String {
    mask_source_text(source, vb, false)
}

fn mask_source_text(source: &str, vb: bool, mask_literals: bool) -> String {
    let input = source.as_bytes();
    let mut output = input.to_vec();
    let mut i = 0;
    let mut block = false;
    let mut quote = None;
    let mut verbatim = false;
    let mut raw_quotes = 0;
    let mut line_prefix_whitespace = true;
    while i < input.len() {
        let c = input[i];
        if c == b'\n' {
            line_prefix_whitespace = true;
            i += 1;
            continue;
        }
        if raw_quotes > 0 {
            let count = input[i..].iter().take_while(|&&c| c == b'"').count();
            if count >= raw_quotes {
                if mask_literals { output[i..i + raw_quotes].fill(b' '); }
                i += raw_quotes;
                raw_quotes = 0;
                continue;
            }
            if mask_literals && !matches!(c, b'\n' | b'\r') {
                output[i] = b' ';
            }
            i += 1;
            continue;
        }
        if block {
            if c == b'*' && input.get(i + 1) == Some(&b'/') {
                output[i..i + 2].fill(b' ');
                i += 2;
                block = false;
                continue;
            }
            if !matches!(c, b'\n' | b'\r') {
                output[i] = b' ';
            }
            i += 1;
            continue;
        }
        if let Some(delimiter) = quote {
            if mask_literals && !matches!(c, b'\n' | b'\r') {
                output[i] = b' ';
            }
            if c == delimiter {
                if (vb || verbatim) && input.get(i + 1) == Some(&delimiter) {
                    if mask_literals { output[i + 1] = b' '; }
                    i += 2;
                    continue;
                }
                quote = None;
            } else if !vb && !verbatim && c == b'\\' && i + 1 < input.len() {
                if mask_literals && !matches!(input[i + 1], b'\n' | b'\r') {
                    output[i + 1] = b' ';
                }
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        let rem = vb
            && line_prefix_whitespace
            && input
                .get(i..i + 3)
                .is_some_and(|s| s.eq_ignore_ascii_case(b"rem"))
            && input.get(i + 3).is_none_or(u8::is_ascii_whitespace);
        if (vb && c == b'\'') || rem || (!vb && c == b'/' && input.get(i + 1) == Some(&b'/')) {
            while i < input.len() && input[i] != b'\n' {
                if input[i] != b'\r' {
                    output[i] = b' ';
                }
                i += 1;
            }
            continue;
        }
        if !vb && c == b'/' && input.get(i + 1) == Some(&b'*') {
            output[i..i + 2].fill(b' ');
            i += 2;
            block = true;
            continue;
        }
        if c == b'"' || (!vb && c == b'\'') {
            let count = input[i..].iter().take_while(|&&c| c == b'"').count();
            if !vb && count >= 3 {
                raw_quotes = count;
                if mask_literals { output[i..i + count].fill(b' '); }
                i += count;
                continue;
            }
            quote = Some(c);
            verbatim = !vb && i > 0 && input[i - 1] == b'@';
            if mask_literals { output[i] = b' '; }
        }
        // VB colon separates statements (including a following Rem comment).
        // Quoted literals were handled above, so their colons do not reset this.
        line_prefix_whitespace = if vb && c == b':' { true }
            else { line_prefix_whitespace && c.is_ascii_whitespace() };
        i += 1;
    }
    String::from_utf8(output).expect("mask preserves source UTF-8")
}

impl OutcomeEvidence {
    pub fn fingerprint(&mut self, body: &str, prompt_version: &str) {
        self.analysis_fingerprint.clear();
        self.analysis_fingerprint = ContentHash::compute(
            format!(
                "{prompt_version}\n{body}\n{}",
                serde_json::to_string(self).expect("evidence serialization")
            )
            .as_bytes(),
        )
        .0;
    }

    pub fn prompt_context(&self) -> String {
        let mut text = format!(
            "\nBounded immediate-return dependencies ({}). Caller source_line anchors remain in the original member. source_verified means declaration/body fingerprints matched, not compiler-verified call binding or certified return/throw outcomes. Project root namespaces, references and conditional compilation are unexamined. For each affected return, require normal completion of the preceding call; never infer that the following return executes merely because it is written. Other helpers/control flow are outside this slice.\n",
            self.scope
        );
        text.push_str(&super::business_reaching_context::prompt_context(self.reaching_context.as_ref()));
        let parameter_source = self.return_paths.as_ref();
        for line in super::business_return_paths::parameter_qualification(parameter_source,
            parameter_source.map(|e| e.source_blake3.as_str()), parameter_source.map(|e| (e.start_line,e.end_line))) {
            text.push_str(&line); text.push('\n');
        }
        if let Some(paths) = &self.return_paths {
            for line in super::business_return_paths::checklist(Some(paths), Some(&paths.source_blake3), Some((paths.start_line,paths.end_line)), 8) {
                text.push_str(&line); text.push('\n');
            }
            text.push_str("Treat distinct source evaluation histories as separate review/test alternatives. Do not collapse them into unconditional fallback rules, infer feasibility, or map a path to a model rule merely by line proximity.\n");
        }
        text.push('\n');
        if let Some(start) = self.reaching_context.as_ref().and_then(|context| context.unexamined_tail_start_line) {
            text.push_str(&format!("Unexamined-tail output contract at caller line {start}: source statements remain visible, but tail outcomes are not established by the bounded scan. Completion of an earlier call is insufficient to assert projection, enumeration, materialization or a later return succeeds. Qualify such proposed outcomes on their own normal completion across rules, steps and error_handling; do not invent tail semantics.\n"));
        }
        if self.execution_stage_contract.as_deref() == Some(EXECUTION_STAGE_CONTRACT) {
            text.push_str(&format!("Execution-stage contract ({EXECUTION_STAGE_CONTRACT}): only operations evaluated before the return value is produced must complete at that point. Returning a deferred query, iterator, delegate, or task does not itself enumerate, invoke, or await the deferred work. Do not assert that deferred work has completed, or must complete before the return, unless the supplied source explicitly executes it. Separately qualify eager calls and unknown helper internals; the declared return type alone does not prove those calls are lazy. Apply this distinction across all six analysis fields.\n"));
        }
        for dependency in &self.dependencies {
            text.push_str(&format!("Dependency at caller line {} before return line {}: {}. Evidence: {}; outcome: {}. {}\n", dependency.call_line, dependency.return_line, dependency.callee_expression, dependency.evidence_status, dependency.outcome_status, dependency.reason));
            text.push_str(&format!("Observed source order: call at caller line {}, following return statement at caller line {}. Outcome constraint: return requires call normal completion and reaching the return; not a return/throw proof.\n",dependency.call_line,dependency.return_line));
            if let Some(body) = &dependency.helper_body {
                text.push_str(&format!(
                    "Helper {} ({}:{}; body hash {}):\n",
                    dependency.helper_identity.as_deref().unwrap_or("unknown"),
                    dependency.helper_file.as_deref().unwrap_or("unknown"),
                    dependency.helper_start_line.unwrap_or(0),
                    dependency.helper_body_hash.as_deref().unwrap_or("unknown")
                ));
                for (offset, line) in body.lines().enumerate() {
                    text.push_str(&format!(
                        "helper {}: {line}\n",
                        dependency.helper_start_line.unwrap_or(1) as usize + offset
                    ));
                }
            }
        }
        for omission in &self.omissions {
            text.push_str(&format!("Omission: {omission}\n"));
        }
        text
    }
}

/// Explicit global qualification only. No suffix, local or receiver guessing.
pub fn collect(
    body: &str,
    owner: &str,
    language: &str,
    start_line: u32,
    file: &str,
    context: Option<&SourceContext<'_>>,
) -> OutcomeEvidence {
    let mut evidence = OutcomeEvidence {
        reaching_context: Some(super::business_reaching_context::collect(body, language, start_line)),
        version: VERSION.into(),
        execution_stage_contract: Some(EXECUTION_STAGE_CONTRACT.into()),
        scope: "standalone_call_immediately_before_return_only; earlier_rule_anchors_conservatively_associated_without_branch_reachability; other_control_flow_unexamined; sequential_validators_return_expressions_async_and_transitive_effects_unexamined; compilation_and_call_binding_not_verified".into(),
        caller_file: file.replace('\\', "/"), caller_owner: owner.into(), caller_start_line: start_line, language: language.into(),
        ..Default::default()
    };
    if !matches!(language, "vb" | "cs" | "csharp") {
        evidence.omissions.push(format!(
            "language {language} is outside the immediate-return scanner"
        ));
        return evidence;
    }
    static CALL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?i)^(?:Call\s+)?((?:global::)?[A-Za-z_]\w*(?:\.[A-Za-z_]\w*)*)\s*\(.*\)\s*;?$",
        )
        .expect("standalone call")
    });
    static RET: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^Return(?:\s|;|$)").expect("return statement"));
    let executable = executable_lines(body, language == "vb");
    let lines: Vec<_> = executable
        .lines()
        .enumerate()
        .map(|(i, line)| (start_line + i as u32, line.trim().to_string()))
        .filter(|(_, line)| !line.is_empty())
        .collect();
    for (i, (line, text)) in lines.iter().enumerate() {
        if !RET.is_match(text) || i == 0 {
            continue;
        }
        let (call_line, call) = &lines[i - 1];
        let Some(capture) = CALL.captures(call) else {
            continue;
        };
        let callee = capture[1].to_string();
        if matches!(
            callee.to_ascii_lowercase().as_str(),
            "if" | "while" | "for" | "switch" | "select"
        ) {
            continue;
        }
        if evidence.dependencies.len() == MAX_DEPENDENCIES {
            evidence
                .omissions
                .push("dependency scan capped at 32; further immediate returns unexamined".into());
            break;
        }
        evidence.dependencies.push(OutcomeDependency {
            callee_expression: callee,
            call_line: *call_line,
            return_line: *line,
            // A lexical nearest-branch heuristic can lose an outer guard after a
            // nested block closes. Without a CFG, keep earlier anchors eligible.
            region_start_line: start_line,
            evidence_status: "not_requested".into(),
            outcome_status: "normal_completion_unverified".into(),
            reason:
                "Helper outcome is not established; following return requires normal completion."
                    .into(),
            helper_identity: None,
            helper_file: None,
            helper_start_line: None,
            helper_end_line: None,
            helper_file_hash: None,
            helper_body_hash: None,
            helper_body: None,
        });
    }
    if evidence.dependencies.is_empty() {
        return evidence;
    }
    let Some(context) = context else {
        return evidence;
    };
    let caller = read_verified_file(context, file);
    let caller_valid = caller.and_then(|(text, _)| {
        let matching = super::business_logic_service::extract_logic_methods(&text, language)
            .into_iter()
            .any(|method| {
                method.start_line == start_line
                    && method.owner == owner
                    && method.body.replace("\r\n", "\n") == body.replace("\r\n", "\n")
            });
        if matching {
            Ok(())
        } else {
            Err("caller source body no longer matches the selected member".into())
        }
    });
    let mut supplied = 0;
    let mut bytes = 0;
    let mut attempts = 0;
    let mut seen: std::collections::HashMap<String, OutcomeDependency> =
        std::collections::HashMap::new();
    for dependency in &mut evidence.dependencies {
        if let Some(prior) = seen.get(&dependency.callee_expression) {
            let (call, ret, region) = (
                dependency.call_line,
                dependency.return_line,
                dependency.region_start_line,
            );
            *dependency = prior.clone();
            dependency.call_line = call;
            dependency.return_line = ret;
            dependency.region_start_line = region;
            dependency.helper_body = None; // Source body was supplied once in this prompt.
            continue;
        }
        if attempts >= MAX_HELPERS {
            dependency.evidence_status = "budget_omitted".into();
            dependency.reason =
                "Four helper-resolution attempts reached; no further helper files read.".into();
            continue;
        }
        attempts += 1;
        let result = caller_valid
            .clone()
            .and_then(|_| resolve(context, owner, language, dependency));
        match result {
            Ok((identity, path, start, end, text, file_hash)) => {
                if supplied >= MAX_HELPERS || bytes + text.len() > MAX_HELPER_BYTES {
                    dependency.evidence_status = "budget_omitted".into();
                    dependency.reason = "Whole helper body exceeds four-helper/8 KiB context budget; outcome unverified.".into();
                    continue;
                }
                supplied += 1;
                bytes += text.len();
                dependency.evidence_status = "source_verified".into();
                dependency.reason = "Helper declaration/body fingerprints matched; compilation and call binding are not verified. Normal completion remains unknown.".into();
                dependency.helper_identity = Some(identity);
                dependency.helper_file = Some(path);
                dependency.helper_start_line = Some(start);
                dependency.helper_end_line = Some(end);
                dependency.helper_file_hash = Some(file_hash);
                dependency.helper_body_hash = Some(ContentHash::compute(text.as_bytes()).0);
                dependency.helper_body = Some(text);
            }
            Err(reason) => {
                dependency.evidence_status = "unavailable".into();
                dependency.reason = reason;
            }
        }
        seen.insert(dependency.callee_expression.clone(), dependency.clone());
    }
    evidence
}

fn read_verified_file(context: &SourceContext<'_>, file: &str) -> Result<(String, String), String> {
    let node = context
        .graph
        .get_node(
            context.project_id,
            &format!("file:{}", file.replace('\\', "/")),
        )
        .map_err(|e| e.to_string())?
        .ok_or("indexed source file missing")?;
    let hash = node
        .metadata
        .as_ref()
        .and_then(|m| m.get("file_hash"))
        .and_then(|v| v.as_str())
        .ok_or("indexed source fingerprint missing")?;
    let bytes = read_bounded_file(context.root, file)?;
    if blake3::hash(&bytes).to_hex().as_str() != hash {
        return Err(format!("source fingerprint stale: {file}"));
    }
    Ok((
        String::from_utf8(bytes).map_err(|_| "source is not UTF-8")?,
        hash.to_string(),
    ))
}

fn read_bounded_file(root: &Path, file: &str) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let path = safe_join(&root, file)
        .map_err(|e| e.to_string())?
        .canonicalize()
        .map_err(|e| e.to_string())?;
    if !path.starts_with(&root) {
        return Err("helper source escapes registered project".into());
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|e| e.to_string())?
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err("helper source file exceeds 8 MiB".into());
    }
    Ok(bytes)
}

fn resolve(
    context: &SourceContext<'_>,
    _owner: &str,
    language: &str,
    dependency: &OutcomeDependency,
) -> Result<(String, String, u32, u32, String, String), String> {
    let name = if language == "vb"
        && dependency
            .callee_expression
            .to_ascii_lowercase()
            .starts_with("global.")
    {
        dependency.callee_expression[7..].to_string()
    } else if language != "vb" && dependency.callee_expression.starts_with("global::") {
        dependency.callee_expression[8..].to_string()
    } else {
        return Err("Only explicit global-qualified static/shared calls can supply helper source; receiver/local/parameter/field/delegate binding is otherwise unverified.".into());
    };
    let nodes = context
        .graph
        .query_nodes(context.project_id, Some("function"), Some(&name), None, 101)
        .map_err(|e| e.to_string())?;
    if nodes.len() > 100 {
        return Err("helper identity lookup capped; binding unknown".into());
    }
    let matches: Vec<_> = nodes
        .into_iter()
        .filter(|node| {
            if language == "vb" {
                node.name.eq_ignore_ascii_case(&name)
            } else {
                node.name == name
            }
        })
        .collect();
    if matches.len() != 1 {
        return Err(format!(
            "helper {name} is absent or ambiguous under exact-name resolution"
        ));
    }
    let node = &matches[0];
    let (source, hash) = read_verified_file(context, node.file_path.as_str())?;
    let members = super::business_logic_service::extract_logic_methods(&source, language);
    let matching: Vec<_> = members
        .into_iter()
        .filter(|member| {
            let fqn = format!("{}.{}", member.owner, member.name);
            member.start_line == node.start_line
                && (if language == "vb" {
                    fqn.eq_ignore_ascii_case(&name)
                } else {
                    fqn == name
                })
        })
        .collect();
    if matching.len() != 1 {
        return Err("helper indexed identity/span does not match source declaration".into());
    }
    let member = &matching[0];
    let declaration = member
        .body
        .lines()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if !declaration
        .split_whitespace()
        .any(|word| word == (if language == "vb" { "shared" } else { "static" }))
    {
        return Err("helper is not an explicitly static/shared source declaration".into());
    }
    let end = member.start_line + member.body.lines().count().saturating_sub(1) as u32;
    if end != node.end_line {
        return Err("helper indexed end line does not match complete source body".into());
    }
    Ok((
        name,
        node.file_path.as_str().to_string(),
        member.start_line,
        end,
        member.body.clone(),
        hash,
    ))
}

pub fn from_document(document: &str) -> Option<OutcomeEvidence> {
    document.lines().find_map(|line| {
        line.strip_prefix("**Outcome dependencies v1**: `")
            .and_then(|text| text.strip_suffix('`'))
            .and_then(|json| serde_json::from_str::<OutcomeEvidence>(json).ok())
            .filter(|evidence| {
                evidence.version == VERSION && evidence.dependencies.len() <= MAX_DEPENDENCIES
            })
    })
}

#[derive(Default)]
pub struct OutcomeAudit {
    hashes: std::collections::HashMap<String, Result<String, String>>,
}

impl OutcomeAudit {
    fn current_hash(&mut self, root: &Path, file: &str) -> Result<String, String> {
        if let Some(result) = self.hashes.get(file) {
            return result.clone();
        }
        if self.hashes.len() >= 8 {
            return Err("helper verification limited to eight distinct files per matrix".into());
        }
        let result =
            read_bounded_file(root, file).map(|bytes| blake3::hash(&bytes).to_hex().to_string());
        self.hashes.insert(file.to_string(), result.clone());
        result
    }
}

pub fn render_dependencies(dependencies: &[OutcomeDependency]) -> String {
    let shorten = |text: &str, limit: usize| -> String {
        let mut result: String = text.chars().take(limit).collect();
        if text.chars().count() > limit {
            result.push_str(" [truncated]");
        }
        result
    };
    let shown: Vec<_> = dependencies
        .iter()
        .take(4)
        .map(|dependency| {
            serde_json::json!({
                "callee": shorten(&dependency.callee_expression, 120),
                "call_line": dependency.call_line, "return_line": dependency.return_line,
                "evidence_status": shorten(&dependency.evidence_status, 64),
                "outcome_status": shorten(&dependency.outcome_status, 64),
                "helper_file": dependency.helper_file.as_deref().map(|file| shorten(file, 240)),
            })
        })
        .collect();
    format!(
        "{}; shown {} of {}, omitted {}. Fetch the full business-logic document with get_chunk for complete dependency identities and reasons.",
        serde_json::to_string(&shown).expect("dependency summaries"),
        shown.len(),
        dependencies.len(),
        dependencies.len().saturating_sub(shown.len())
    )
}

/// Match anchors conservatively to a scanned region; never upgrade semantics.
pub fn for_rule(
    evidence: Option<&OutcomeEvidence>,
    rule: &str,
    root: &Path,
) -> (String, Vec<OutcomeDependency>) {
    for_rule_with_audit(evidence, rule, root, &mut OutcomeAudit::default())
}

pub fn for_rule_with_audit(
    evidence: Option<&OutcomeEvidence>,
    rule: &str,
    root: &Path,
    audit: &mut OutcomeAudit,
) -> (String, Vec<OutcomeDependency>) {
    let Some(evidence) = evidence else {
        return ("legacy_dependency_coverage_unknown".into(), Vec::new());
    };
    static ANCHOR: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\[line (\d+)\]").expect("rule anchor"));
    let anchor = ANCHOR
        .captures(rule)
        .and_then(|capture| capture[1].parse::<u32>().ok());
    let mut dependencies: Vec<_> = evidence
        .dependencies
        .iter()
        .filter(|dependency| {
            anchor.is_none_or(|line| {
                dependency.region_start_line <= line && line <= dependency.return_line
            })
        })
        .cloned()
        .collect();
    for dependency in &mut dependencies {
        if let (Some(file), Some(hash)) = (&dependency.helper_file, &dependency.helper_file_hash) {
            if !audit
                .current_hash(root, file)
                .is_ok_and(|current| &current == hash)
            {
                dependency.evidence_status = "stale_or_unavailable".into();
                dependency.reason = "Helper source changed or is unavailable; refresh the analysis before using its outcome.".into();
            }
        }
    }
    let status = if !dependencies.is_empty() {
        "blocked_pending_helper_outcome"
    } else if !evidence.omissions.is_empty() {
        "dependency_coverage_incomplete"
    } else {
        "inferred_unverified_outside_dependency_slice"
    };
    (status.into(), dependencies)
}

#[cfg(test)]
mod tests {
    #[test]
    fn execution_stage_contract_is_fingerprinted_and_legacy_absence_is_preserved() {
        let body = "Function Read() As IQueryable(Of Row)\nReturn Build().Where(Function(x) x.Enabled)\nEnd Function";
        let mut current = super::collect(body, "Reader", "vb", 1, "Reader.vb", None);
        assert_eq!(current.execution_stage_contract.as_deref(), Some(super::EXECUTION_STAGE_CONTRACT));
        current.fingerprint(body, "unchanged-member-template");
        let mut legacy_json = serde_json::to_value(&current).unwrap();
        legacy_json.as_object_mut().unwrap().remove("execution_stage_contract");
        let mut legacy: super::OutcomeEvidence = serde_json::from_value(legacy_json).unwrap();
        assert!(legacy.execution_stage_contract.is_none());
        assert!(!legacy.prompt_context().contains("Execution-stage contract"));
        legacy.fingerprint(body, "unchanged-member-template");
        assert_ne!(current.analysis_fingerprint, legacy.analysis_fingerprint);
        let prompt = current.prompt_context();
        assert!(prompt.contains("does not itself enumerate, invoke, or await"));
        assert!(prompt.contains("declared return type alone does not prove those calls are lazy"));
        assert!(prompt.contains("all six analysis fields"));
    }

    #[test]
    fn prompt_contract_distinguishes_recorded_pairs_from_unexamined_sites() {
        let simple = super::collect("Return 1", "Reader", "vb", 50, "Reader.vb", None);
        let prompt = simple.prompt_context();
        assert!(!prompt.contains("Observed source order:"));
        assert!(!prompt.contains("Unexamined-tail output contract"));
        let pair = super::collect("Notify()\nReturn Nothing", "Reader", "vb", 50, "Reader.vb", None);
        let prompt = pair.prompt_context();
        assert!(prompt.contains("Observed source order: call at caller line 50, following return statement at caller line 51"));
        assert!(prompt.contains("return requires call normal completion and reaching the return"));
        assert!(prompt.contains("not a return/throw proof"));
        assert!(!prompt.contains("Unexamined-tail output contract"));
        assert_eq!(pair.dependencies[0].outcome_status, "normal_completion_unverified");
    }

    use super::*;

    #[test]
    fn nested_block_closure_does_not_clear_outer_rule_dependency() {
        for (language, body) in [
            (
                "vb",
                "Function Read() As Object\nIf outer Then\nIf inner Then\nWork()\nEnd If\nGlobal.Helpers.Reject()\nReturn Nothing\nEnd If\nEnd Function",
            ),
            (
                "cs",
                "object Read() {\nif (outer) {\nif (inner) {\nWork();\n}\nglobal::Helpers.Reject();\nreturn null;\n}\n}",
            ),
        ] {
            let evidence = collect(body, "Caller", language, 10, "Caller", None);
            assert_eq!(evidence.dependencies.len(), 1);
            let (status, deps) = for_rule(
                Some(&evidence),
                "IF outer THEN returns empty [line 11]",
                Path::new("."),
            );
            assert_eq!(status, "blocked_pending_helper_outcome", "{language}");
            assert_eq!(deps.len(), 1);
            assert!(evidence.scope.contains("without_branch_reachability"));
        }
    }
    use engram_core::RelPath;
    use engram_graph::Node;
    const CALLER: &str = "Public Class Caller\n Public Function ReadItem(invalid As Boolean) As Object\n  If invalid Then\n   Global.Helpers.Reject()\n   Return Nothing\n  End If\n  Return New Object()\n End Function\nEnd Class\n";
    const HELPER: &str = "Public Class Helpers\n Public Shared Sub Reject()\n  Throw New InvalidOperationException()\n End Sub\nEnd Class\n";
    fn index(graph: &GraphStore, root: &Path, file: &str, source: &str) {
        std::fs::write(root.join(file), source).unwrap();
        let mut nodes = vec![Node {
            node_id: format!("file:{file}"),
            node_type: "file".into(),
            name: file.into(),
            namespace: "memory".into(),
            language: "vb".into(),
            file_path: RelPath::new(file),
            start_line: 1,
            end_line: source.lines().count() as u32,
            generation: 1,
            metadata: Some(
                serde_json::json!({"file_hash":blake3::hash(source.as_bytes()).to_hex().to_string()}),
            ),
        }];
        for member in super::super::business_logic_service::extract_logic_methods(source, "vb") {
            nodes.push(Node {
                node_id: format!("{file}:{}:{}", member.name, member.start_line),
                node_type: "function".into(),
                name: format!("{}.{}", member.owner, member.name),
                namespace: "memory".into(),
                language: "vb".into(),
                file_path: RelPath::new(file),
                start_line: member.start_line,
                end_line: member.start_line + member.body.lines().count() as u32 - 1,
                generation: 1,
                metadata: None,
            });
        }
        graph.upsert_nodes("p", &nodes).unwrap();
    }
    fn collected(graph: &GraphStore, root: &Path) -> OutcomeEvidence {
        let member =
            super::super::business_logic_service::extract_logic_methods(CALLER, "vb").remove(0);
        collect(
            &member.body,
            &member.owner,
            "vb",
            member.start_line,
            "Caller.vb",
            Some(&SourceContext {
                graph,
                project_id: "p",
                root,
            }),
        )
    }
    #[test]
    fn exact_helper_source_is_not_a_verified_outcome_and_staleness_changes_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let graph = GraphStore::open(&tmp.path().join("graph.redb")).unwrap();
        index(&graph, tmp.path(), "Caller.vb", CALLER);
        index(&graph, tmp.path(), "Helpers.vb", HELPER);
        let mut evidence = collected(&graph, tmp.path());
        assert_eq!(evidence.dependencies.len(), 1);
        let dependency = &evidence.dependencies[0];
        assert_eq!(dependency.evidence_status, "source_verified");
        assert_eq!(dependency.outcome_status, "normal_completion_unverified");
        assert_eq!((dependency.call_line, dependency.return_line), (4, 5));
        assert!(
            dependency
                .helper_body
                .as_ref()
                .unwrap()
                .contains("Throw New")
        );
        evidence.fingerprint(CALLER, "v2");
        let fingerprint = evidence.analysis_fingerprint.clone();
        let document = format!(
            "**Outcome dependencies v1**: `{}`",
            serde_json::to_string(&evidence).unwrap()
        );
        let stored = from_document(&document).unwrap();
        assert!(
            stored.dependencies[0].helper_body.is_none(),
            "do not duplicate helper bodies in persisted docs"
        );
        let (status, dependencies) = for_rule(
            Some(&stored),
            "IF invalid THEN returns Nothing [line 3]",
            tmp.path(),
        );
        assert_eq!(status, "blocked_pending_helper_outcome");
        assert_eq!(dependencies[0].evidence_status, "source_verified");
        std::fs::write(
            tmp.path().join("Helpers.vb"),
            HELPER.replace("Throw New InvalidOperationException()", "Return"),
        )
        .unwrap();
        let (_, dependencies) = for_rule(
            Some(&stored),
            "IF invalid THEN returns Nothing [line 3]",
            tmp.path(),
        );
        assert_eq!(dependencies[0].evidence_status, "stale_or_unavailable");
        let mut stale = collected(&graph, tmp.path());
        stale.fingerprint(CALLER, "v2");
        assert_ne!(fingerprint, stale.analysis_fingerprint);
        assert_eq!(stale.dependencies[0].evidence_status, "unavailable");
        index(
            &graph,
            tmp.path(),
            "Helpers.vb",
            &HELPER.replace("Throw New InvalidOperationException()", "Return"),
        );
        let returning = collected(&graph, tmp.path());
        assert_eq!(returning.dependencies[0].evidence_status, "source_verified");
        assert_eq!(
            returning.dependencies[0].outcome_status,
            "normal_completion_unverified"
        );
    }
    #[test]
    fn ambiguous_or_missing_helpers_are_never_guessed() {
        let tmp = tempfile::tempdir().unwrap();
        let graph = GraphStore::open(&tmp.path().join("graph.redb")).unwrap();
        index(&graph, tmp.path(), "Caller.vb", CALLER);
        assert_eq!(
            collected(&graph, tmp.path()).dependencies[0].evidence_status,
            "unavailable"
        );
        index(&graph, tmp.path(), "Helpers.vb", HELPER);
        index(&graph, tmp.path(), "Duplicate.vb", HELPER);
        let evidence = collected(&graph, tmp.path());
        assert_eq!(evidence.dependencies[0].evidence_status, "unavailable");
        assert!(evidence.dependencies[0].reason.contains("ambiguous"));
    }
    #[test]
    fn nonadjacent_calls_and_unexamined_paths_do_not_gain_outcome_certainty() {
        let evidence = collect(
            "Function ReadItem() As Object\n Helpers.Reject()\n Dim x = 1\n Return Nothing\nEnd Function",
            "Caller",
            "vb",
            1,
            "Caller.vb",
            None,
        );
        assert!(evidence.dependencies.is_empty());
        assert!(evidence.scope.contains("other_control_flow_unexamined"));
        let unresolved = collect(
            "Function ReadItem() As Object\n Helpers.Reject()\n Return Nothing\nEnd Function",
            "Caller",
            "vb",
            1,
            "Caller.vb",
            None,
        );
        assert_eq!(unresolved.dependencies[0].evidence_status, "not_requested");
        assert_eq!(
            for_rule(None, "legacy", Path::new(".")).0,
            "legacy_dependency_coverage_unknown"
        );
    }
    #[test]
    fn comments_and_multiline_literals_cannot_supply_dependencies() {
        for (language, source) in [
            (
                "vb",
                "Function ReadItem() As Object\n' Helpers.Reject()\n Return Nothing\nEnd Function",
            ),
            (
                "vb",
                "Function ReadItem() As Object\n Dim text = \"Helpers.Reject()\n Return Nothing\"\nEnd Function",
            ),
            (
                "cs",
                "object ReadItem() {\n/*\n Helpers.Reject();\n return null;\n*/\n return null;\n}",
            ),
            (
                "cs",
                "object ReadItem() {\n var text = @\"\n Helpers.Reject();\n return null;\n\";\n return null;\n}",
            ),
            (
                "cs",
                "object ReadItem() {\n var text = \"\"\"\n Helpers.Reject();\n return null;\n\"\"\";\n return null;\n}",
            ),
        ] {
            assert!(
                collect(source, "Caller", language, 1, "Caller", None)
                    .dependencies
                    .is_empty(),
                "{source}"
            );
        }
        let source = "object ReadItem() {\n Helpers.Reject(); // might throw\n return null;\n}";
        let evidence = collect(source, "Caller", "cs", 1, "Caller.cs", None);
        assert_eq!(evidence.dependencies.len(), 1);
        assert_eq!(
            (
                evidence.dependencies[0].call_line,
                evidence.dependencies[0].return_line
            ),
            (2, 3)
        );
    }
    #[test]
    fn shadowed_receivers_and_local_delegates_never_supply_class_source() {
        let tmp = tempfile::tempdir().unwrap();
        let graph = GraphStore::open(&tmp.path().join("graph.redb")).unwrap();
        index(&graph, tmp.path(), "Helpers.vb", HELPER);
        for source in [
            CALLER
                .replace(
                    "ReadItem(invalid As Boolean)",
                    "ReadItem(invalid As Boolean, Helpers As Object)",
                )
                .replace("Global.Helpers.Reject()", "Helpers.Reject()"),
            CALLER
                .replace(
                    "Public Class Caller",
                    "Public Class Caller\n Private Helpers As Object",
                )
                .replace("Global.Helpers.Reject()", "Helpers.Reject()"),
            CALLER.replace("Global.Helpers.Reject()", "Reject()"),
        ] {
            index(&graph, tmp.path(), "Caller.vb", &source);
            let member = super::super::business_logic_service::extract_logic_methods(&source, "vb")
                .remove(0);
            let result = collect(
                &member.body,
                &member.owner,
                "vb",
                member.start_line,
                "Caller.vb",
                Some(&SourceContext {
                    graph: &graph,
                    project_id: "p",
                    root: tmp.path(),
                }),
            );
            assert_eq!(result.dependencies[0].evidence_status, "unavailable");
            assert!(result.dependencies[0].helper_body.is_none());
        }
    }
    #[tokio::test]
    async fn library_cache_round_trip_uses_dependency_fingerprint_not_caller_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let graph = GraphStore::open(&tmp.path().join("graph.redb")).unwrap();
        index(&graph, tmp.path(), "Caller.vb", CALLER);
        index(&graph, tmp.path(), "Helpers.vb", HELPER);
        let member =
            super::super::business_logic_service::extract_logic_methods(CALLER, "vb").remove(0);
        let mut evidence = collected(&graph, tmp.path());
        evidence.return_paths = super::super::business_return_paths::collect(
            "Caller.vb", CALLER, &member.body, member.start_line, "vb",
        ).await;
        evidence.fingerprint(
            &member.body,
            super::super::business_logic_service::MEMBER_PROMPT_VERSION,
        );
        let doc = format!(
            "**Outcome dependencies v1**: `{}`",
            serde_json::to_string(&evidence).unwrap()
        );
        let stored = from_document(&doc).unwrap();
        let key = format!("Caller.vb|Caller.ReadItem|{:?}", member.overload_line);
        let mut cache =
            std::collections::HashMap::from([(key.clone(), stored.analysis_fingerprint)]);
        let context = SourceContext {
            graph: &graph,
            project_id: "p",
            root: tmp.path(),
        };
        let engine = engram_ml::DreamingEngine::new();
        let (_, analyzed, skipped) =
            super::super::business_logic_service::analyze_file_logic_with_context(
                &engine,
                "Caller.vb",
                CALLER,
                &cache,
                Some(&context),
            )
            .await;
        assert_eq!((analyzed, skipped), (0, 1));
        cache.insert(key, ContentHash::compute(member.body.as_bytes()).0);
        let (_, _, skipped) =
            super::super::business_logic_service::analyze_file_logic_with_context(
                &engine,
                "Caller.vb",
                CALLER,
                &cache,
                Some(&context),
            )
            .await;
        assert_eq!(
            skipped, 0,
            "legacy caller-only hashes cannot reuse helper-aware analysis"
        );
        // The MCP handler intentionally supplies an empty cache. This test
        // verifies the library contract, not a new persisted MCP reuse path.
    }
    #[test]
    fn rendered_dependency_details_are_capped_without_losing_stored_records() {
        let mut evidence = collect(
            "Function X() As Object\n Global.Helpers.Reject()\n Return Nothing\nEnd Function",
            "Caller",
            "vb",
            1,
            "Caller.vb",
            None,
        );
        let mut dependency = evidence.dependencies[0].clone();
        dependency.callee_expression = "z".repeat(20_000);
        dependency.reason = "r".repeat(20_000);
        evidence.dependencies = vec![dependency; 32];
        let rendered = render_dependencies(&evidence.dependencies);
        assert!(rendered.contains("shown 4 of 32, omitted 28"));
        assert!(rendered.contains("get_chunk"));
        assert!(rendered.len() < 2500);
        assert_eq!(evidence.dependencies.len(), 32);
    }
    #[tokio::test]
    async fn moving_an_unchanged_member_invalidates_numbered_anchor_cache() {
        let source = "Public Class ConstantValue\n Public Function Read() As Integer\n  Return 42\n End Function\nEnd Class\n";
        let member =
            super::super::business_logic_service::extract_logic_methods(source, "vb").remove(0);
        let mut evidence = collect(
            &member.body,
            &member.owner,
            "vb",
            member.start_line,
            "ConstantValue.vb",
            None,
        );
        assert!(evidence.dependencies.is_empty());
        evidence.return_paths = super::super::business_return_paths::collect(
            "ConstantValue.vb", source, &member.body, member.start_line, "vb",
        ).await;
        evidence.fingerprint(
            &member.body,
            super::super::business_logic_service::MEMBER_PROMPT_VERSION,
        );
        let cache = std::collections::HashMap::from([(
            format!(
                "ConstantValue.vb|ConstantValue.Read|{:?}",
                member.overload_line
            ),
            evidence.analysis_fingerprint,
        )]);
        let engine = engram_ml::DreamingEngine::new();
        let (_, _, skipped) = super::super::business_logic_service::analyze_file_logic(
            &engine,
            "ConstantValue.vb",
            source,
            &cache,
        )
        .await;
        assert_eq!(skipped, 1);
        let moved = format!("\n\n{source}");
        let moved_member =
            super::super::business_logic_service::extract_logic_methods(&moved, "vb").remove(0);
        assert_eq!(member.body, moved_member.body);
        assert_ne!(member.start_line, moved_member.start_line);
        let (_, _, skipped) = super::super::business_logic_service::analyze_file_logic(
            &engine,
            "ConstantValue.vb",
            &moved,
            &cache,
        )
        .await;
        assert_eq!(
            skipped, 0,
            "old numbered anchors must not survive a member move"
        );
    }
}

#[cfg(test)]
mod parameter_prompt_tests {
    use super::*;
    #[test]
    fn prompt_negative_occurrence_fact_and_fingerprint_bind_optional_evidence() {
        let mut e = OutcomeEvidence { language:"vb".into(), return_paths:Some(super::super::business_return_paths::ReturnPathEvidence {
            source_blake3:"raw-source".into(),start_line:2,end_line:8,unavailable_reason:None,
            result:Some(serde_json::json!({"version":"vb-return-paths-v1","method_start_line":2,"method_end_line":8,
                "status":"unavailable","structural_complete":false,"paths":[],
                "parameter_occurrences":{"version":"vb-parameter-occurrences-v1","status":"available","scanned_body_tokens":8,
                    "parameters":[{"identifier":"optionFlag","declaration_line":2,"body_identifier_occurrences":0}]}}))}), ..Default::default() };
        let prompt=e.prompt_context();
        assert!(prompt.contains("parameter \"optionFlag\" declared at line 2 has zero body identifier occurrences"));
        assert!(prompt.contains("Do not assert a body read"));
        assert!(prompt.contains("zero is not runtime-unused proof"));
        let version=super::super::business_logic_service::MEMBER_PROMPT_VERSION;
        e.fingerprint("same body",version);let initial=e.analysis_fingerprint.clone();
        e.return_paths.as_mut().unwrap().result.as_mut().unwrap()["parameter_occurrences"]["parameters"][0]["body_identifier_occurrences"]=serde_json::json!(1);
        e.fingerprint("same body",version);assert_ne!(initial,e.analysis_fingerprint);
        assert!(!e.prompt_context().contains("Source token fact:"));
        e.return_paths=None;
        assert!(e.prompt_context().contains("UNKNOWN (VB-only"));
    }
}
