//! Compatibility of produced source chunks and extraction metadata.

/// The extraction contract actually used to produce one ingestion result.
/// Runtime sidecar availability/identity is recorded separately per VB file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceExtractionIdentity {
    pub source_index_version: u64,
    pub target_chars_per_chunk: usize,
}

/// Request/admission-local expectation. Probe once off the async executor;
/// neither this receipt nor module MVIDs attest project references/options.
#[derive(Debug, Clone)]
pub struct SourceExtractionExpectation {
    configured: SourceExtractionIdentity,
    vb_parser: Option<crate::vb_extractor::VbParserContext>,
}

impl SourceExtractionExpectation {
    pub fn for_paths<'a>(target_chars_per_chunk: usize, paths: impl IntoIterator<Item = &'a str>) -> anyhow::Result<Self> {
        let vb_parser = if paths.into_iter().any(crate::vb_extractor::requires_parser_provenance) {
            Some(crate::vb_extractor::current_parser_context()?)
        } else { None };
        Ok(Self { configured: SourceExtractionIdentity::for_chunk_target(target_chars_per_chunk), vb_parser })
    }

    pub fn metadata_is_compatible(&self, metadata: &serde_json::Value, path: &str) -> bool {
        let Some(configured) = metadata.get("source_extraction_identity").and_then(serde_json::Value::as_object)
            else { return false; };
        if metadata.get("source_index_version").and_then(|v| v.as_u64()) != Some(crate::SOURCE_INDEX_VERSION)
            || configured.len() != 2
            || configured.get("source_index_version").and_then(|v| v.as_u64()) != Some(self.configured.source_index_version)
            || configured.get("target_chars_per_chunk").and_then(|v| v.as_u64())
                .and_then(|value| usize::try_from(value).ok()) != Some(self.configured.target_chars_per_chunk) { return false; }
        if crate::webforms::is_webforms_markup(std::path::Path::new(path))
            && metadata.get("webforms_position_contract").and_then(serde_json::Value::as_u64)
                != Some(crate::webforms::POSITION_CONTRACT) { return false; }
        if !crate::vb_extractor::requires_parser_provenance(path) { return true; }
        self.vb_parser.as_ref().is_some_and(|expected| metadata.get("vb_parser_provenance")
            .is_some_and(|actual| expected.matches_metadata(actual)))
    }

    /// Confirm actual production against the expectation that admitted the
    /// whole scan, including inherited files. Stable degraded backends under
    /// that same context are reusable but remain explicitly labelled fallback.
    pub fn verify_produced(&self, stats: &crate::IngestStats) -> anyhow::Result<()> {
        match stats.source_extraction_identity {
            Some(actual) => anyhow::ensure!(actual == self.configured,
                "Source chunk/extraction configuration changed after admission; rescan and retry"),
            None => anyhow::ensure!(stats.files == 0 && stats.chunks == 0 && stats.bytes == 0
                && stats.all_files.is_empty() && stats.fingerprints.is_empty() && stats.symbols.is_empty()
                && stats.edges.is_empty() && stats.vb_parser_provenance.is_empty()
                && stats.webforms_position_contracts.is_empty(),
                "Produced source output has no captured extraction configuration"),
        }
        for (path, actual) in &stats.vb_parser_provenance {
            anyhow::ensure!(actual.is_verified() && actual.context() == self.vb_parser.as_ref(),
                "VB parser context changed while producing {}; source publication refused, rescan and retry", path);
        }
        for (path, contract) in &stats.webforms_position_contracts {
            anyhow::ensure!(*contract == crate::webforms::POSITION_CONTRACT
                && crate::webforms::is_webforms_markup(std::path::Path::new(path))
                && stats.all_files.iter().any(|produced| produced.as_str() == path),
                "WebForms position receipt does not belong to current produced output: {}", path);
        }
        for path in &stats.all_files {
            if crate::webforms::is_webforms_markup(std::path::Path::new(path.as_str())) {
                anyhow::ensure!(stats.webforms_position_contracts.get(path.as_str())
                    == Some(&crate::webforms::POSITION_CONTRACT),
                    "Produced WebForms file {} has no current position contract", path.as_str());
            }
            if !crate::vb_extractor::requires_parser_provenance(path.as_str()) { continue; }
            anyhow::ensure!(stats.vb_parser_provenance.contains_key(path.as_str()),
                "Produced VB file {} has no parser provenance", path.as_str());
        }
        Ok(())
    }

    /// Run the synchronous registry commit under the same parser coordinator
    /// that validates its captured code/policy context. The closure must not
    /// await or reenter extraction. Registry source-identity CAS remains required.
    pub fn with_publication_fence<T>(&self, publish: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
        match &self.vb_parser {
            Some(expected) => crate::vb_extractor::with_parser_publication_fence(expected, publish),
            None => publish(),
        }
    }
}

impl SourceExtractionIdentity {
    pub const fn for_chunk_target(target_chars_per_chunk: usize) -> Self {
        Self {
            source_index_version: crate::SOURCE_INDEX_VERSION,
            target_chars_per_chunk,
        }
    }
}

#[cfg(test)]
mod publication_tests;
