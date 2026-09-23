//! Hash-bound validation of external code-generator receipts.
//!
//! Engram does not invoke an IDE or generator. It verifies a small, portable
//! receipt against the registered project's current files so a successful
//! generator run cannot be represented by an unbound console claim.

use std::io::Read;
use std::path::{Path, PathBuf};

use rmcp::ErrorData as McpError;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const MAX_RECEIPT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GeneratorReceipt {
    #[serde(alias = "SourceFile")]
    source_file: String,
    #[serde(alias = "CustomTool", alias = "tool")]
    generator: String,
    #[serde(alias = "Invoked")]
    invoked: bool,
    #[serde(default, alias = "ProjectName")]
    project_name: Option<String>,
    #[serde(default, alias = "SolutionFile")]
    solution_file: Option<String>,
    #[serde(default, alias = "VisualStudioVersion")]
    host_version: Option<String>,
    #[serde(default, alias = "Files")]
    files: Vec<GeneratorFileReceipt>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GeneratorFileReceipt {
    #[serde(alias = "Path")]
    path: String,
    #[serde(alias = "ExistedBefore")]
    existed_before: bool,
    #[serde(alias = "ExistsAfter")]
    exists_after: bool,
    #[serde(alias = "LengthBefore")]
    length_before: u64,
    #[serde(alias = "LengthAfter")]
    length_after: u64,
    #[serde(default, alias = "Sha256Before")]
    sha256_before: Option<String>,
    #[serde(default, alias = "Sha256After")]
    sha256_after: Option<String>,
    #[serde(alias = "Changed")]
    changed: bool,
}

/// Result consumed by `validate_generated_code`. A malformed request is an MCP
/// parameter error; a well-formed receipt whose claims no longer match disk is
/// a normal failing validation check.
#[derive(Debug, Clone)]
pub struct GeneratorReceiptEvidence {
    pub status: &'static str,
    pub details: Vec<String>,
}

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

fn valid_hex64(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:X}", Sha256::digest(bytes))
}

fn relative_identity(path: &str) -> String {
    path.replace('\\', "/")
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect::<Vec<_>>()
        .join("/")
}

fn receipt_path_in_root(root: &Path, raw: &str) -> Result<(PathBuf, String), String> {
    let root = std::fs::canonicalize(root)
        .map_err(|error| format!("cannot resolve registered project root: {error}"))?;
    let candidate = Path::new(raw);
    let full = if candidate.is_absolute() {
        std::fs::canonicalize(candidate)
            .map_err(|error| format!("cannot resolve receipt file `{raw}`: {error}"))?
    } else {
        let safe = engram_core::safe_join(&root, raw).map_err(|error| error.to_string())?;
        std::fs::canonicalize(safe)
            .map_err(|error| format!("cannot resolve receipt file `{raw}`: {error}"))?
    };
    if !full.starts_with(&root) {
        return Err(format!(
            "receipt file escapes the registered project root: {raw}"
        ));
    }
    let relative = full
        .strip_prefix(&root)
        .map_err(|_| format!("receipt file escapes the registered project root: {raw}"))?
        .to_string_lossy()
        .to_string();
    Ok((full, relative_identity(&relative)))
}

fn current_fingerprint(root: &Path, raw: &str) -> Result<(String, u64, String), String> {
    let (full, relative) = receipt_path_in_root(root, raw)?;
    let metadata = std::fs::metadata(&full)
        .map_err(|error| format!("cannot inspect receipt member `{raw}`: {error}"))?;
    if !metadata.is_file() {
        return Err(format!("receipt member is not a regular file: {raw}"));
    }
    let bytes = std::fs::read(&full)
        .map_err(|error| format!("cannot read receipt member `{raw}`: {error}"))?;
    Ok((relative, metadata.len(), sha256(&bytes)))
}

fn changed_claim_is_consistent(file: &GeneratorFileReceipt) -> bool {
    let observed = file.existed_before != file.exists_after
        || file.length_before != file.length_after
        || !option_hash_eq(&file.sha256_before, &file.sha256_after);
    observed == file.changed
}

fn option_hash_eq(left: &Option<String>, right: &Option<String>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
        (None, None) => true,
        _ => false,
    }
}

fn validate_receipt(
    root: &Path,
    receipt: GeneratorReceipt,
    target_file: &str,
) -> GeneratorReceiptEvidence {
    let mut failures = Vec::new();
    let mut warnings = Vec::new();
    if !receipt.invoked {
        failures.push("receipt says the generator was not invoked".to_string());
    }
    if receipt.generator.trim().is_empty() {
        failures.push("receipt does not identify the generator/tool".to_string());
    }
    if receipt.source_file.trim().is_empty() {
        failures.push("receipt does not identify the generator source file".to_string());
    }
    if receipt.files.is_empty() {
        failures.push("receipt contains no before/after file fingerprints".to_string());
    }

    let target_identity = relative_identity(target_file).to_lowercase();
    let source_identity = match receipt_path_in_root(root, &receipt.source_file) {
        Ok((_, identity)) => Some(identity),
        Err(error) => {
            failures.push(format!("invalid generator source: {error}"));
            None
        }
    };
    let mut source_seen = false;
    let mut target_seen = false;
    let mut verified_files = 0usize;
    for file in &receipt.files {
        if file.existed_before {
            match file.sha256_before.as_deref() {
                Some(hash) if valid_hex64(hash) => {}
                _ => failures.push(format!(
                    "receipt member `{}` existed before but has no valid before SHA-256",
                    file.path
                )),
            }
        } else if file.length_before != 0 || file.sha256_before.is_some() {
            failures.push(format!(
                "receipt member `{}` did not exist before but carries before length/hash data",
                file.path
            ));
        }
        if !file.exists_after && (file.length_after != 0 || file.sha256_after.is_some()) {
            failures.push(format!(
                "receipt member `{}` does not exist after but carries after length/hash data",
                file.path
            ));
        }
        if !changed_claim_is_consistent(file) {
            failures.push(format!(
                "changed flag is inconsistent with before/after data for `{}`",
                file.path
            ));
        }
        if !file.exists_after {
            warnings.push(format!("generator removed receipt member `{}`", file.path));
            continue;
        }
        let Some(after_hash) = file.sha256_after.as_deref() else {
            failures.push(format!(
                "receipt member `{}` has no after SHA-256",
                file.path
            ));
            continue;
        };
        if !valid_hex64(after_hash) {
            failures.push(format!(
                "receipt member `{}` has an invalid after SHA-256",
                file.path
            ));
            continue;
        }
        match current_fingerprint(root, &file.path) {
            Ok((identity, length, current_hash)) => {
                verified_files += 1;
                if length != file.length_after || !current_hash.eq_ignore_ascii_case(after_hash) {
                    failures.push(format!(
                        "current file `{identity}` does not match the receipt's after length/SHA-256"
                    ));
                }
                if identity.eq_ignore_ascii_case(&target_identity) {
                    target_seen = true;
                }
                if source_identity
                    .as_deref()
                    .is_some_and(|source| identity.eq_ignore_ascii_case(source))
                {
                    source_seen = true;
                }
            }
            Err(error) => failures.push(error),
        }
    }
    if !source_seen {
        failures.push(
            "generator source is not covered by a matching current-file fingerprint".to_string(),
        );
    }
    if !target_seen {
        failures.push(format!(
            "target file `{target_file}` is not covered by the generator receipt"
        ));
    }

    if !failures.is_empty() {
        failures.extend(warnings);
        return GeneratorReceiptEvidence {
            status: "fail",
            details: failures,
        };
    }
    let has_warnings = !warnings.is_empty();
    let mut details = vec![format!(
        "Generator `{}` was invoked; {} current file fingerprint(s), including source and target, match the hash-bound receipt",
        receipt.generator, verified_files
    )];
    let host = [
        receipt.project_name,
        receipt.solution_file,
        receipt.host_version,
    ]
    .into_iter()
    .flatten()
    .filter(|value| !value.trim().is_empty())
    .collect::<Vec<_>>();
    if !host.is_empty() {
        details.push(format!("Receipt host identity: {}", host.join(" | ")));
    }
    details.extend(warnings);
    GeneratorReceiptEvidence {
        status: if has_warnings { "warn" } else { "pass" },
        details,
    }
}

pub async fn resolve(
    engram: &crate::tools::Engram,
    project_id: &str,
    receipt_file: Option<&str>,
    expected_sha256: Option<&str>,
    target_file: Option<&str>,
    code_file: Option<&str>,
) -> Result<Option<GeneratorReceiptEvidence>, McpError> {
    let (receipt_file, expected_sha256) = match (receipt_file, expected_sha256) {
        (None, None) => return Ok(None),
        (Some(file), Some(hash)) => (file, hash),
        _ => {
            return Err(invalid(
                "generator_receipt_file and generator_receipt_sha256 must be supplied together",
            ));
        }
    };
    if !valid_hex64(expected_sha256) {
        return Err(invalid(
            "generator_receipt_sha256 must be 64 hexadecimal characters",
        ));
    }
    if Path::new(receipt_file).is_absolute()
        || receipt_file.contains(':')
        || receipt_file.starts_with("\\\\")
    {
        return Err(invalid("generator_receipt_file must be project-relative"));
    }
    let Some(target_file) = target_file else {
        return Err(invalid("generator receipt validation requires target_file"));
    };
    let Some(code_file) = code_file else {
        return Err(invalid(
            "generator receipt validation requires hash-bound code_file input, not inline code",
        ));
    };
    if !relative_identity(code_file).eq_ignore_ascii_case(&relative_identity(target_file)) {
        return Err(invalid(
            "generator receipt target_file must match code_file exactly",
        ));
    }

    let record = engram.ensure_project_record(project_id).await?;
    let root = PathBuf::from(record.directory);
    let receipt_file = receipt_file.to_owned();
    let expected_sha256 = expected_sha256.to_owned();
    let target_file = target_file.to_owned();
    tokio::task::spawn_blocking(move || {
        let (full, _) = receipt_path_in_root(&root, &receipt_file).map_err(invalid)?;
        let mut file = std::fs::File::open(&full).map_err(|error| invalid(error.to_string()))?;
        let mut bytes = Vec::new();
        file.by_ref()
            .take((MAX_RECEIPT_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|error| invalid(error.to_string()))?;
        if bytes.len() > MAX_RECEIPT_BYTES {
            return Err(invalid(
                "generator receipt exceeds 1 MiB; no partial receipt is checked",
            ));
        }
        let actual = sha256(&bytes);
        if !actual.eq_ignore_ascii_case(&expected_sha256) {
            return Err(invalid(
                "generator receipt SHA-256 mismatch; refresh the exact raw-byte hash",
            ));
        }
        // Windows-hosted IDE/PowerShell writers commonly emit a UTF-8 BOM.
        // Keep it inside the caller-bound raw SHA-256, then remove only those
        // three standard bytes for JSON decoding.
        let json_bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
        let receipt: GeneratorReceipt = serde_json::from_slice(json_bytes)
            .map_err(|error| invalid(format!("invalid generator receipt JSON: {error}")))?;
        Ok(validate_receipt(&root, receipt, &target_file))
    })
    .await
    .map_err(|error| McpError::internal_error(error.to_string(), None))?
    .map(Some)
}
