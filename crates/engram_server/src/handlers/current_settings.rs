//! Explicit current-file observations independent of indexed/compiler evidence.
use engram_index::vb_extractor::literal_settings::{Observation, source_observations};
use serde::Serialize;
use std::{collections::BTreeSet, io::Read, path::Path};

#[derive(Serialize)]
pub(super) struct Site {
    pub file_path: String,
    pub raw_blake3: String,
    #[serde(flatten)]
    pub observation: Observation,
}
#[derive(Serialize)]
pub(super) struct SourceFile {
    pub file_path: String,
    pub raw_blake3: String,
    pub observed_at_ms: u64,
}
#[derive(Serialize)]
pub(super) struct Report {
    pub format: &'static str,
    pub evidence_kind: &'static str,
    pub project_id: String,
    pub query: Option<String>,
    pub status: &'static str,
    pub source_files: Vec<SourceFile>,
    pub observations: Vec<Site>,
    pub incomplete: Vec<String>,
    pub interpretation: &'static str,
}

pub(super) fn validate(files: &[String]) -> anyhow::Result<()> {
    anyhow::ensure!(
        !files.is_empty() && files.len() <= 16,
        "source_files requires1..16 exact project-relative VB files"
    );
    for file in files {
        let path = file.replace('\\', "/");
        anyhow::ensure!(
            path.len() <= 2048
                && !path.chars().any(char::is_control)
                && !path.starts_with('/')
                && !path.contains(':')
                && !path
                    .split('/')
                    .any(|part| part.is_empty() || part == "." || part == "..")
                && Path::new(&path)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("vb")),
            "source_files requires exact project-relative VB files without traversal: {file}"
        );
    }
    Ok(())
}

pub(super) fn collect(
    root: &Path,
    project_id: &str,
    files: &[String],
    query: Option<&str>,
    limit: usize,
    budget: &engram_core::memory::MemoryBudget,
) -> anyhow::Result<Report> {
    validate(files)?;
    anyhow::ensure!(
        query.is_none_or(|value| !value.trim().is_empty()),
        "setting name must not be blank"
    );
    let _allocation = engram_core::memory::AllocationGuard::try_new(
        budget,
        32 * 1024 * 1024,
        engram_core::memory::Subsystem::Graph,
        "current literal settings source observations",
    )?;
    let mut result = Report {
        format: "engram-current-settings-v1",
        evidence_kind: "current_file_literal_getter_observations",
        project_id: project_id.into(),
        query: query.map(str::to_owned),
        status: "no_matches_in_supported_syntax",
        source_files: Vec::new(),
        observations: Vec::new(),
        incomplete: Vec::new(),
        interpretation: "Only explicitly named current files were read. These are same-line, complete-literal, name-shaped getter observations, not compiler/store binding, runtime values, indexed axes or verified business rules. Computed/multiline keys and unsupported getters remain unknown. File reads are independent, not one atomic snapshot. The index is unchanged. Propose QA values only after reviewing the actual task and consuming source; no tests were executed.",
    };
    let mut seen = BTreeSet::new();
    for file in files {
        let path = engram_core::safe_join(root, file)?;
        let canonical = path.canonicalize()?;
        if !seen.insert(canonical) {
            continue;
        }
        anyhow::ensure!(
            std::fs::metadata(&path)?.len() <= 2 * 1024 * 1024,
            "current settings source exceeds2MiB/file: {file}"
        );
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(2 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        anyhow::ensure!(
            bytes.len() <= 2 * 1024 * 1024,
            "current settings source exceeds2MiB/file: {file}"
        );
        let hash = blake3::hash(&bytes).to_hex().to_string();
        let source = String::from_utf8(bytes)
            .map_err(|_| anyhow::anyhow!("current settings requires UTF-8 VB source: {file}"))?;
        result.source_files.push(SourceFile {
            file_path: file.clone(),
            raw_blake3: hash.clone(),
            observed_at_ms: crate::utils::now_ms(),
        });
        let (rows, truncated) = source_observations(
            &source,
            limit.saturating_sub(result.observations.len()),
            query,
        );
        for row in rows {
            if row.key.len() > 512 || row.name.len() > 1024 {
                result.incomplete.push(format!("{file}:{}: literal key/name exceeds display bounds; observation omitted, inspect the source file", row.line));
                continue;
            }
            result.observations.push(Site {
                file_path: file.clone(),
                raw_blake3: hash.clone(),
                observation: row,
            });
        }
        if truncated {
            result.incomplete.push(format!("{file}: matching observations capped at{limit} across this request; omitted matches remain unknown"));
        }
    }
    if !result.observations.is_empty() {
        result.status = "observations_available";
    }
    let names: BTreeSet<_> = result
        .observations
        .iter()
        .map(|row| &row.observation.name)
        .collect();
    if query.is_some() && names.len() > 1 {
        result.status = "ambiguous_use_exact_catalog_label";
    }
    Ok(result)
}
