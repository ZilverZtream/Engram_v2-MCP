//! Source-linked proposed tests from version-checked inferred requirements.
use super::business_source::SourceAudit;
use engram_index::{HybridQuery, HybridSearchEngine};
use std::{collections::HashSet, path::Path};

/// Presentation-only correction for historical setting-shaped edges. Only a
/// source-proven, uniquely declared private nullable field qualifies; a real
/// resolved setting or a qualified configuration/member chain stays a setting.
pub(super) fn nullable_member_observation(
    source_code: Option<&str>,
    file: &str,
    target: &str,
) -> Option<String> {
    use regex::Regex;
    if !file.to_ascii_lowercase().ends_with(".vb") {
        return None;
    }
    let code = source_code?;
    let name = target.strip_prefix("::")?.strip_suffix(".HasValue")?;
    if !Regex::new(r"^[A-Za-z_]\w*$").ok()?.is_match(name) {
        return None;
    }
    let escaped = regex::escape(name);
    // Unrelated nested types do not invalidate an outer field. Each use must
    // still belong to the exact class containing its unique declaration.
    let type_scopes = nullable_type_scopes(code)?;
    let declarations = Regex::new(&format!(r"(?i)\b{escaped}\s+As\b")).ok()?;
    if declarations.find_iter(&code).count() != 1 {
        return None;
    }
    let declaration = Regex::new(&format!(r"(?im)^\s*Private\s+(?:Shared\s+|ReadOnly\s+)?{escaped}\s+As\s+(?:(?:Boolean|Byte|SByte|Short|UShort|Integer|UInteger|Long|ULong|Single|Double|Decimal|Date)\s*\?|(?:System\.)?Nullable\s*\(\s*Of\s+(?:Boolean|Byte|SByte|Short|UShort|Integer|UInteger|Long|ULong|Single|Double|Decimal|Date)\s*\))\s*(?:=|$)")).ok()?;
    let decl = declaration.find(&code)?;
    let name_offset = decl.start()
        + decl
            .as_str()
            .to_ascii_lowercase()
            .find(&name.to_ascii_lowercase())?;
    let scope_at = |offset| {
        type_scopes
            .iter()
            .find(|(start, end, _)| *start <= offset && offset < *end)
            .and_then(|(_, _, scope)| *scope)
    };
    let declaration_scope = scope_at(name_offset)?;
    let parameter = Regex::new(&format!(
        r"(?im)^.*\b(?:Sub|Function|Property)\s+[^\r\n]*\([^\r\n]*\b{escaped}\b"
    ))
    .ok()?;
    if parameter.is_match(&code) {
        return None;
    }
    // Also retain uncertainty for inferred locals, loop/query variables and
    // lambda parameters that need not carry a second explicit As clause.
    let shadows = Regex::new(&format!(r"(?im)\b(?:For(?:\s+Each)?|Catch|Using|From|Let)\s+{escaped}\b|\b(?:Function|Sub)\s*\([^\r\n)]*\b{escaped}\b")).ok()?;
    let locals = Regex::new(&format!(
        r"(?i)\b(?:Private|Dim)\s+(?:Shared\s+|ReadOnly\s+)?{escaped}\b"
    ))
    .ok()?;
    if shadows.is_match(code) || locals.find_iter(code).count() != 1 {
        return None;
    }
    let uses = Regex::new(&format!(r"(?i)\b{escaped}\s*\.\s*HasValue\b")).ok()?;
    let mut lines = Vec::new();
    let mut previous_offset = 0;
    let mut line = 1;
    for usage in uses.find_iter(&code) {
        if scope_at(usage.start()) != Some(declaration_scope) {
            return None;
        }
        // A prefix receiver would make this a different member chain.
        if code[..usage.start()].trim_end().ends_with('.') {
            return None;
        }
        line += code[previous_offset..usage.start()]
            .bytes()
            .filter(|b| *b == b'\n')
            .count();
        previous_offset = usage.start();
        if lines.last() != Some(&line) {
            lines.push(line);
        }
    }
    if lines.is_empty() {
        return None;
    }
    let decl_line = code[..name_offset].bytes().filter(|b| *b == b'\n').count() + 1;
    let omitted = lines.len().saturating_sub(8);
    let sites = lines
        .iter()
        .take(8)
        .map(|line| format!("{file}:{line}"))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "{name}.HasValue: source-declared private nullable field (declaration {file}:{decl_line}); presence check at {sites}. Not a configuration-setting identity or cross-page state proof; value origin and access outcome remain unassessed. Additional use lines omitted: {omitted}."
    ))
}

/// Line ranges and exact lexical class identity in already masked VB source.
/// Other nested type bodies remain distinct, and malformed/inline declarations
/// are withheld. This does not resolve inheritance, local variables or receivers.
fn nullable_type_scopes(code: &str) -> Option<Vec<(usize, usize, Option<usize>)>> {
    use regex::Regex;
    let opening = Regex::new(r"(?i)^[\t ]*(?:(?:Public|Private|Friend|Protected|Partial|MustInherit|NotInheritable|Shadows)\s+)*(Class|Structure|Module|Interface|Enum)\s+\S+").ok()?;
    let closing = Regex::new(r"(?i)^End[\t ]+(Class|Structure|Module|Interface|Enum)$").ok()?;
    let mut stack: Vec<(String, usize)> = Vec::new();
    let mut ranges = Vec::new();
    let mut offset = 0;
    for line in code.split_inclusive('\n') {
        if let Some(caps) = opening.captures(line) {
            if line.contains(':') || line.trim_end().ends_with('_') {
                return None;
            }
            stack.push((caps[1].to_ascii_lowercase(), offset));
        }
        let class = stack
            .last()
            .and_then(|(kind, id)| (kind == "class").then_some(*id));
        ranges.push((offset, offset + line.len(), class));
        if let Some(caps) = closing.captures(line.trim()) {
            let (kind, _) = stack.pop()?;
            if !kind.eq_ignore_ascii_case(&caps[1]) {
                return None;
            }
        }
        offset += line.len();
    }
    stack.is_empty().then_some(ranges)
}

/// A dangling target has no reliable use location merely because the graph's
/// owner is a file node. Supply a concrete, bounded recovery query instead.
pub(super) fn setting_use_recovery(pid: &str, file: &str, target: &str) -> String {
    let token = target.strip_prefix("::").unwrap_or(target);
    format!(
        "Source-use recovery: grep_project({}) (literal token search; inspect matches, not a proven setting binding).",
        serde_json::json!({"project_id":pid,"pattern":token,"path_prefix":file,"max_results":20,"output_json":true,"regex":false})
    )
}

/// Known stale graph axes are withheld. Legacy axes remain explicitly
/// unverified so a missing fingerprint cannot masquerade as current evidence.
pub(super) fn axis_source(
    graph: &engram_graph::GraphStore,
    pid: &str,
    root: &Path,
    file: &str,
    bytes_read: &mut u64,
) -> (bool, Option<String>, Option<Vec<u8>>) {
    use std::io::Read;
    let verify = || -> Result<Option<String>, String> {
        let node = graph
            .get_node(pid, &format!("file:{file}"))
            .map_err(|e| e.to_string())?;
        let metadata = node.and_then(|node| node.metadata);
        if let Some(version) = metadata
            .as_ref()
            .and_then(|meta| meta.get("source_index_version"))
            .and_then(serde_json::Value::as_u64)
        {
            if version < engram_index::SOURCE_INDEX_VERSION {
                return Err(format!(
                    "indexed axes use obsolete source format {version}; update_project must re-extract this file before its dependencies can be used"
                ));
            }
        }
        Ok(metadata.and_then(|meta| {
            meta.get("file_hash")
                .and_then(|value| value.as_str())
                .map(str::to_owned)
        }))
    };
    let hash = match verify() {
        Ok(Some(hash)) => hash,
        Ok(None) => {
            return (
                true,
                Some(format!(
                    "{file}: indexed axes are UNVERIFIED because the file fingerprint is absent"
                )),
                None,
            );
        }
        Err(error) => {
            return (
                false,
                Some(format!("{file}: axis fingerprint lookup failed: {error}")),
                None,
            );
        }
    };
    let read = || -> Result<Vec<u8>, String> {
        let path = engram_core::safe_join(root, file)
            .map_err(|e| e.to_string())?
            .canonicalize()
            .map_err(|e| e.to_string())?;
        let root = root.canonicalize().map_err(|e| e.to_string())?;
        if !path.starts_with(root) {
            return Err("source escapes the registered project".into());
        }
        let size = std::fs::metadata(&path).map_err(|e| e.to_string())?.len();
        if size > 8 * 1024 * 1024 || bytes_read.saturating_add(size) > 64 * 1024 * 1024 {
            return Err("axis verification budget exceeded (8 MiB/file, 64 MiB/query)".into());
        }
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .map_err(|e| e.to_string())?
            .take(8 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() > 8 * 1024 * 1024
            || bytes_read.saturating_add(bytes.len() as u64) > 64 * 1024 * 1024
        {
            return Err("axis verification budget exceeded while reading".into());
        }
        Ok(bytes)
    };
    match read() {
        Ok(bytes) => {
            *bytes_read += bytes.len() as u64;
            // file_hash is the indexer's raw-byte fingerprint. ContentHash
            // normalizes line endings for chunks and falsely rejects CRLF files.
            if blake3::hash(&bytes).to_hex().as_str() == hash {
                (true, None, Some(bytes))
            } else {
                (
                    false,
                    Some(format!(
                        "{file}: STALE graph axes withheld because source changed; run update_project before deriving indexed axes"
                    )),
                    None,
                )
            }
        }
        Err(error) => (
            false,
            Some(format!(
                "{file}: axis source verification unavailable: {error}"
            )),
            None,
        ),
    }
}

/// Resolve only an explicit code-behind directive in the verified markup
/// snapshot. No filename guessing or transitive helper expansion.
pub(super) fn declared_axis_companion(
    root: &Path,
    file: &str,
    snapshot: &[u8],
) -> Result<Option<String>, String> {
    if !matches!(
        Path::new(file)
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("aspx" | "ascx" | "master")
    ) {
        return Ok(None);
    }
    let markup = std::str::from_utf8(snapshot).map_err(|_| {
        "markup encoding is not UTF-8; code-behind declaration not examined".to_string()
    })?;
    let Some(declared) = crate::services::validation_mapping_service::declared_codebehind(markup)
    else {
        return Ok(None);
    };
    let declared = declared.replace('\\', "/");
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let page = engram_core::safe_join(&root, file).map_err(|e| e.to_string())?;
    let candidate = if let Some(relative) = declared.strip_prefix("~/") {
        super::access_layer_tools::discover_web_application_root(&root, &page).join(relative)
    } else {
        page.parent().unwrap_or(&root).join(&declared)
    };
    let candidate = candidate
        .canonicalize()
        .map_err(|e| format!("declared code-behind {declared} is unavailable: {e}"))?;
    let relative = candidate
        .strip_prefix(&root)
        .map_err(|_| "declared code-behind escapes the registered project".to_string())?;
    if !candidate.is_file()
        || !matches!(
            candidate
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("vb" | "cs")
        )
    {
        return Err(format!(
            "declared code-behind {declared} is not a supported VB/C# source file"
        ));
    }
    Ok(Some(relative.to_string_lossy().replace('\\', "/")))
}

pub(super) struct RuleCase {
    pub requirement: String,
    pub doc_id: String,
    pub source_status: String,
    pub source_warnings: Vec<String>,
    pub expected_outcome_status: String,
    pub other_outcome_status: String,
    pub source_diagnostic_qualification: String,
    pub reaching_prerequisites_require_review: bool,
    pub reaching_qualification: String,
    pub outcome_dependencies:
        Vec<crate::services::business_outcome_dependencies::OutcomeDependency>,
}

/// Warnings describe the analyzed document. Their rule numbers are local to
/// that document, not the case numbers assigned by a multi-document matrix.
fn source_check_warnings(document: &str) -> Vec<String> {
    let mut in_checks = false;
    let mut warnings: Vec<String> = Vec::new();
    for line in document.lines() {
        if line.starts_with("_Source: ") {
            in_checks = false;
        }
        if line.starts_with("## ") {
            in_checks = line.trim() == "## Source checks requiring review";
            continue;
        }
        if !in_checks || line.trim().is_empty() {
            continue;
        }
        if let Some(warning) = line.trim().strip_prefix("- ") {
            warnings.push(warning.to_string());
        } else if let Some(warning) = warnings.last_mut() {
            warning.push('\n');
            warning.push_str(line.trim());
        } else {
            warnings.push(line.trim().to_string());
        }
    }
    let total = warnings.len();
    warnings.truncate(20);
    for warning in &mut warnings {
        if warning.chars().count() > 1000 {
            *warning = warning.chars().take(1000).collect::<String>()
                + " [warning truncated; fetch full source document]";
        }
    }
    if total > 20 {
        warnings.push(format!(
            "{} additional source-check warnings omitted; fetch full source document",
            total - 20
        ));
    }
    warnings
}

pub(super) fn collect(
    search: &HybridSearchEngine,
    pid: &str,
    generation: u64,
    files: &[String],
    root: &Path,
) -> Result<(Vec<RuleCase>, Vec<String>), String> {
    let query = HybridQuery {
        project_id: pid.into(),
        namespace: "business_logic".into(),
        generation,
        text: "*".into(),
        top_k: 201,
        fts_mode: "regex".into(),
        include_path_prefixes: Some(
            files
                .iter()
                .map(|file| format!("__business_logic/{file}/"))
                .collect(),
        ),
        exclude_path_prefixes: None,
        include_path_suffixes: None,
        language_filters: None,
        author_filter: None,
        date_after: None,
        date_before: None,
        use_mmr: false,
    };
    let mut hits = search
        .lexical_search(&query)
        .map_err(|error| error.to_string())?;
    let mut notes = Vec::new();
    if hits.len() > 200 {
        notes.push("rule document coverage truncated at 200; narrow the requested files".into());
        hits.truncate(200);
    }
    let mut seen = HashSet::new();
    let mut audit = SourceAudit::default();
    let mut cases = Vec::new();
    let mut outcome_audit = crate::services::business_outcome_dependencies::OutcomeAudit::default();
    for hit in hits {
        if !seen.insert(hit.doc_id.clone()) {
            continue;
        }
        let Some((_, _, document, _, _)) = search
            .get_doc_by_doc_id(pid, "business_logic", 0, &hit.doc_id)
            .map_err(|error| error.to_string())?
        else {
            notes.push(format!("{}: full rule document unavailable", hit.doc_id));
            continue;
        };
        let source = document.lines().find_map(|line| {
            line.strip_prefix("_Source: ")
                .and_then(|source| source.strip_suffix('_'))
        });
        if !source.is_some_and(|source| files.iter().any(|file| file == &source.replace('\\', "/")))
        {
            notes.push(format!(
                "{}: rule source does not match requested file scope",
                hit.doc_id
            ));
            continue;
        }
        let source_warnings = source_check_warnings(&document);
        let rule_diagnostics = crate::services::business_rule_diagnostics::from_document(&document);
        let warning_notes = || {
            source_warnings.iter().map(|warning| format!(
            "{}: analysis source check (document-local rule numbers): {warning}; recovery: get_chunk(doc_id=\"{}\", namespace=\"business_logic\")",
            hit.doc_id, hit.doc_id,
        )).collect::<Vec<_>>()
        };
        let status = audit.describe(&document, root);
        if !status.starts_with("VERIFIED_METHOD_HASH:") {
            notes.push(format!("{}: {status}", hit.doc_id));
            notes.extend(warning_notes());
            continue;
        }
        let outcome_evidence =
            crate::services::business_outcome_dependencies::from_document(&document);
        // Independent source alternatives; never attach them to inferred rules by proximity.
        // Eight paths per document, exact source/anchor checks, remaining paths recoverable.
        notes.extend(
            audit
                .return_path_checklist(&document, root, 8)
                .into_iter()
                .map(|note| format!("{}: {note}", hit.doc_id)),
        );
        notes.push(format!(
            "{}: full_document: get_chunk({})",
            hit.doc_id,
            serde_json::json!({"project_id":pid,"namespace":"business_logic","doc_id":hit.doc_id})
        ));
        notes.extend(
            super::business_source::claim_review_guidance(pid, &hit.doc_id, &document)
                .into_iter()
                .map(|note| format!("{}: {note}", hit.doc_id)),
        );
        let cases_before_document = cases.len();
        let exact_rules = crate::services::business_rule_diagnostics::exact_document_rules(
            rule_diagnostics.as_ref(),
            &document,
        );
        if exact_rules.is_none() && rule_diagnostics.is_some() {
            notes.push(format!("{}: exact stored rule-boundary recovery unavailable (legacy/mismatched/ambiguous block or 400-rule/128-KiB recovery budget); using legacy line display without guessing diagnostic identity. Full rules: get_chunk for this document.", hit.doc_id));
        }
        let rules = exact_rules.unwrap_or_else(|| {
            let mut in_rules = false;
            document
                .lines()
                .filter_map(|line| {
                    if line.starts_with("## ") {
                        in_rules = line.trim() == "## Business Rules";
                        return None;
                    }
                    if !in_rules {
                        return None;
                    }
                    let rule = line.trim().strip_prefix("- ")?;
                    (!rule.trim().is_empty()).then(|| rule.to_owned())
                })
                .collect()
        });
        for rule in &rules {
            if cases.len() >= 40 {
                if cases.len() == cases_before_document {
                    notes.extend(warning_notes());
                }
                notes.push(
                    "proposed case output truncated at 40; narrow the requested files".into(),
                );
                return Ok((cases, notes));
            }
            let mut requirement: String = rule.chars().take(1000).collect();
            if rule.chars().count() > 1000 {
                requirement.push_str(" [truncated; fetch source document]");
            }
            let (expected_outcome_status, outcome_dependencies) =
                crate::services::business_outcome_dependencies::for_rule_with_audit(
                    outcome_evidence.as_ref(),
                    rule,
                    root,
                    &mut outcome_audit,
                );
            let reaching = crate::services::business_reaching_context::qualify(
                outcome_evidence
                    .as_ref()
                    .and_then(|e| e.reaching_context.as_ref()),
                Some(rule),
            );
            let expected_outcome_status = if reaching.has_prerequisites
                && expected_outcome_status != "blocked_pending_helper_outcome"
            {
                "blocked_pending_reaching_prerequisite_review".to_owned()
            } else {
                expected_outcome_status
            };
            let source_diagnostic = crate::services::business_rule_diagnostics::associate(
                rule_diagnostics.as_ref(),
                rule,
            );
            let other_outcome_status = expected_outcome_status.clone();
            let expected_outcome_status = if source_diagnostic.blocks_outcome {
                "blocked_pending_source_validation".to_owned()
            } else {
                expected_outcome_status
            };
            cases.push(RuleCase {
                requirement,
                doc_id: hit.doc_id.clone(),
                source_status: status.clone(),
                source_warnings: source_warnings.clone(),
                expected_outcome_status,
                other_outcome_status,
                source_diagnostic_qualification: source_diagnostic.summary,
                reaching_prerequisites_require_review: reaching.has_prerequisites,
                reaching_qualification: reaching.summary,
                outcome_dependencies,
            });
        }
        if cases.len() == cases_before_document {
            notes.extend(warning_notes());
        }
    }
    Ok((cases, notes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::{Config, ContentHash, RelPath};
    use engram_index::IndexDoc;

    #[tokio::test]
    async fn proposed_cases_require_matching_source_hash_and_exact_file_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let source = "Class Rules\nPublic Function Save(value As Integer) As String\nIf value < 0 Then Return \"invalid\"\nReturn \"ok\"\nEnd Function\nEnd Class\n";
        std::fs::write(tmp.path().join("Rules.vb"), source).unwrap();
        let method =
            crate::services::business_logic_service::extract_logic_methods(source, "vb").remove(0);
        let body_hash = ContentHash::compute(method.body.as_bytes()).0;
        let content = format!(
            "# Rules.Save\n**Analysis method hash**: `{body_hash}`\n## Source checks requiring review\n- Rule 1: cited source line is a comment; inspect it.\n## Business Rules\n- Negative quantities return a validation error.\n_Source: Rules.vb_\n"
        );
        let cfg = Config {
            embedding_backend: "fts_only".into(),
            ..Default::default()
        };
        let search =
            HybridSearchEngine::new(tmp.path().join("fts"), tmp.path().join("vectors"), &cfg)
                .await
                .unwrap();
        let mut docs = Vec::new();
        for (index, content) in [
            content.clone(),
            content.replace("_Source: Rules.vb_", "_Source: Other.vb_"),
        ]
        .into_iter()
        .enumerate()
        {
            docs.push(IndexDoc {
                generation: 0,
                chunk_id: index as u64,
                path: RelPath::new(&format!("__business_logic/Rules.vb/Save{index}.md")),
                language: "markdown".into(),
                content_hash: ContentHash::compute(content.as_bytes()).0,
                content,
                namespace: "business_logic".into(),
                author: None,
                timestamp: None,
                start_line: 0,
                end_line: 0,
                doc_id: format!("rule{index}"),
            });
        }
        search
            .index_docs("test", &docs, &tokio_util::sync::CancellationToken::new())
            .await
            .unwrap();
        let (cases, notes) = collect(&search, "test", 1, &["Rules.vb".into()], tmp.path()).unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(
            cases[0].source_warnings,
            ["Rule 1: cited source line is a comment; inspect it."]
        );
        assert_eq!(
            cases[0].requirement,
            "Negative quantities return a validation error."
        );
        assert!(notes.iter().any(|note| note.contains("file scope")));
        std::fs::write(
            tmp.path().join("Rules.vb"),
            source.replace("value < 0", "value > 0"),
        )
        .unwrap();
        let (cases, notes) = collect(&search, "test", 1, &["Rules.vb".into()], tmp.path()).unwrap();
        assert!(cases.is_empty());
        assert!(notes.iter().any(|note| note.contains("STALE:")));
        assert!(notes
            .iter()
            .any(|note| note.contains("cited source line is a comment")));
    }
}
