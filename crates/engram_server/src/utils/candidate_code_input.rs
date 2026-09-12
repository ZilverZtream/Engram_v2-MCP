//! Exact buffered file input for the existing three snippet-check tools.
use std::io::Read;
use std::path::Path;

use rmcp::{ErrorData as McpError, model::{CallToolResult, Content}};
use serde_json::{Value, json};

pub const MAX_CODE_BYTES: usize = 4 * 1024 * 1024;

pub struct ResolvedCodeInput {
    pub code: String,
    pub context: Option<String>,
    pub evidence: Option<Value>,
}

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

fn relative_identity(path: &str) -> String {
    path.replace('\\', "/").split('/').filter(|p| !p.is_empty() && *p != ".").collect::<Vec<_>>().join("/")
}

fn resolve_file(root: &Path, project_id: &str, path: &str, expected: &str, context: Option<&str>) -> Result<ResolvedCodeInput, McpError> {
    if expected.len() != 64 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid("code_file_blake3 must be 64 hexadecimal characters"));
    }
    engram_core::safe_join(root, path).map_err(|e| invalid(e.to_string()))?;
    let identity = relative_identity(path);
    if let Some(context) = context {
        engram_core::safe_join(root, context).map_err(|e| invalid(e.to_string()))?;
        if relative_identity(context) != identity {
            return Err(invalid("File input context must match code_file exactly; no basename or content aliases"));
        }
    }
    let file = engram_core::safe_open_read(root, path).map_err(|e| invalid(e.to_string()))?;
    if !file.metadata().map_err(|e| invalid(e.to_string()))?.is_file() {
        return Err(invalid("code_file must be a regular file"));
    }
    let mut bytes = Vec::new();
    file.take((MAX_CODE_BYTES + 1) as u64).read_to_end(&mut bytes).map_err(|e| invalid(e.to_string()))?;
    if bytes.len() > MAX_CODE_BYTES { return Err(invalid("code_file exceeds 4 MiB; no partial input is checked")); }
    let digest = blake3::hash(&bytes).to_hex().to_string();
    if !digest.eq_ignore_ascii_case(expected) { return Err(invalid("code_file BLAKE3 mismatch; refresh the exact raw-byte hash")); }
    let byte_length = bytes.len();
    let code = String::from_utf8(bytes).map_err(|_| invalid("code_file must be UTF-8; no lossy conversion"))?;
    if code.trim().is_empty() { return Err(invalid("code_file must contain nonblank code")); }
    Ok(ResolvedCodeInput { code, context: Some(identity.clone()), evidence: Some(json!({
        "kind":"project_file", "project_id":project_id, "project_relative_path":identity,
        "byte_length":byte_length, "raw_blake3":digest,
        "scope":"exact buffered UTF-8 bytes; no later file immutability or semantic approval"
    })) })
}

pub async fn resolve(engram: &crate::tools::Engram, project_id: &str, code: Option<&str>, path: Option<&str>, expected: Option<&str>, context: Option<&str>) -> Result<ResolvedCodeInput, McpError> {
    match (code, path, expected) {
        (Some(code), None, None) => Ok(ResolvedCodeInput { code: code.to_owned(), context: context.map(str::to_owned), evidence: None }),
        (None, Some(path), Some(expected)) => {
            let record = engram.ensure_project_record(project_id).await?;
            let root = std::path::PathBuf::from(record.directory);
            let (project_id, path, expected, context) = (project_id.to_owned(), path.to_owned(), expected.to_owned(), context.map(str::to_owned));
            tokio::task::spawn_blocking(move || resolve_file(&root, &project_id, &path, &expected, context.as_deref()))
                .await.map_err(|e| McpError::internal_error(e.to_string(), None))?
        }
        _ => Err(invalid("Provide code OR code_file + code_file_blake3; inputs must not be mixed or incomplete")),
    }
}

/// Preserve inline output; attach read identity even to corpus-empty/degraded or error exits.
pub fn attach(result: Result<CallToolResult, McpError>, evidence: Option<Value>, output_json: bool) -> Result<CallToolResult, McpError> {
    let Some(evidence) = evidence else { return result; };
    let mut result = match result {
        Ok(result) => result,
        Err(mut error) => {
            error.data = Some(json!({"input_evidence":evidence,"original_error_data":error.data.take()}));
            return Err(error);
        }
    };
    if output_json && let Some(text) = result.content.first().and_then(|c| c.as_text()) {
        let mut parsed: Value = serde_json::from_str(&text.text).map_err(|_| McpError::internal_error("Expected existing JSON quality response", Some(json!({"input_evidence":evidence}))))?;
        let object = parsed.as_object_mut().ok_or_else(|| McpError::internal_error("Expected JSON quality object", Some(json!({"input_evidence":evidence.clone()}))))?;
        object.insert("input_evidence".into(), evidence);
        result.content[0] = Content::text(parsed.to_string());
    } else {
        let identity = format!("\n\nInput evidence: {}", evidence);
        if let Some(text) = result.content.first().and_then(|c| c.as_text()) {
            result.content[0] = Content::text(format!("{}{identity}", text.text));
        } else {
            result.content.push(Content::text(identity));
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_utf8_and_terminal_newlines_bind_without_normalization() {
        let temp = tempfile::tempdir().unwrap();
        let body = "\u{feff}class Caf\u{e9} {}\r\n";
        std::fs::write(temp.path().join("Code.cs"), body).unwrap();
        let digest = blake3::hash(body.as_bytes()).to_hex().to_string();
        let input = resolve_file(temp.path(), "p", "Code.cs", &digest, None).unwrap();
        assert_eq!(input.code, body);
        assert_eq!(input.evidence.as_ref().unwrap()["byte_length"], body.len());
        assert_eq!(input.evidence.as_ref().unwrap()["project_relative_path"], "Code.cs");
        for changed in [body.trim_end().to_string(), body.replace("\r\n", "\n")] {
            assert!(resolve_file(temp.path(), "p", "Code.cs", &blake3::hash(changed.as_bytes()).to_hex().to_string(), None).is_err());
        }
    }
    #[test]
    fn same_content_wrong_context_unsafe_non_utf8_and_size_inputs_fail() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("a.cs"), "x").unwrap();
        std::fs::write(temp.path().join("b.cs"), "x").unwrap();
        let hash = blake3::hash(b"x").to_hex().to_string();
        assert!(resolve_file(temp.path(), "p", "a.cs", &hash, Some("b.cs")).is_err());
        let other_project = tempfile::tempdir().unwrap();
        assert!(resolve_file(other_project.path(), "other", "a.cs", &hash, None).is_err());
        for path in ["../a.cs", "/a.cs", "missing.cs", ""] { assert!(resolve_file(temp.path(), "p", path, &hash, None).is_err()); }
        assert!(resolve_file(temp.path(), "p", "a.cs", "invalid", None).is_err());
        for body in [vec![0xff], Vec::new(), vec![b'x'; MAX_CODE_BYTES + 1]] {
            std::fs::write(temp.path().join("a.cs"), &body).unwrap();
            assert!(resolve_file(temp.path(), "p", "a.cs", &blake3::hash(&body).to_hex().to_string(), None).is_err());
        }
        let body = vec![b'x'; MAX_CODE_BYTES];
        std::fs::write(temp.path().join("a.cs"), &body).unwrap();
        assert_eq!(resolve_file(temp.path(), "p", "a.cs", &blake3::hash(&body).to_hex().to_string(), None).unwrap().code.len(), MAX_CODE_BYTES);
    }
    #[cfg(unix)]
    #[test]
    fn file_mode_does_not_follow_a_link_out_of_project() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("a.cs"), "x").unwrap();
        std::os::unix::fs::symlink(outside.path().join("a.cs"), root.path().join("a.cs")).unwrap();
        assert!(resolve_file(root.path(), "p", "a.cs", &blake3::hash(b"x").to_hex().to_string(), None).is_err());
    }
    #[test]
    fn evidence_survives_degraded_results_and_errors_without_changing_inline() {
        let text = "verdict: INSUFFICIENT\ncomparison: not_run";
        let inline = attach(Ok(CallToolResult::success(vec![Content::text(text)])), None, false).unwrap();
        assert_eq!(inline.content[0].as_text().unwrap().text, text);
        let evidence = json!({"byte_length":2,"raw_blake3":"digest","project_relative_path":"a.cs"});
        let file = attach(Ok(inline), Some(evidence.clone()), false).unwrap();
        let rendered = &file.content[0].as_text().unwrap().text;
        assert!(rendered.starts_with(text));assert!(rendered.contains("Input evidence:"));
        let error = attach(Err(invalid("existing failure")), Some(evidence.clone()), false).unwrap_err();
        assert_eq!(error.message, "existing failure");
        assert_eq!(error.data.unwrap()["input_evidence"], evidence);
        for invalid_json in ["[]", "null", "not-json"] {
            let error = attach(Ok(CallToolResult::success(vec![Content::text(invalid_json)])), Some(evidence.clone()), true).unwrap_err();
            assert_eq!(error.data.unwrap()["input_evidence"], evidence);
        }
    }
}
