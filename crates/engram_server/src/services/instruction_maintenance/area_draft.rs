//! New area files remain proposals. Evidence integrity is checked mechanically;
//! claim classification, applicability and policy acceptance require review.
use super::*;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NewTarget {
    pub instruction_path: String,
    pub provenance_path: String,
    /// Proposed project-relative applyTo patterns; no inferred widening.
    pub area_scope: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AbsentTarget {
    pub target: NewTarget,
    pub before: &'static str,
    pub after: &'static str,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Section {
    Purpose,
    Invariant,
    HouseExample,
    CompanionFile,
    AcceptedException,
    EntryPoint,
    Consumer,
    ValidationPath,
}

impl Section {
    fn heading(&self) -> &'static str {
        match self {
            Self::Purpose => "Purpose",
            Self::Invariant => "Invariants",
            Self::HouseExample => "House examples",
            Self::CompanionFile => "Companion files",
            Self::EntryPoint => "Entry points",
            Self::Consumer => "Consumers",
            Self::ValidationPath => "Validation paths",
            Self::AcceptedException => "Exceptions and cited rulings — review required",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClaimKind {
    ObservedImplementation,
    AuthoredIntent,
    ProposedPolicy,
}

impl ClaimKind {
    fn label(&self) -> &'static str {
        match self {
            Self::ObservedImplementation => "Observed implementation; not a requirement",
            Self::AuthoredIntent => "Authored intent; confirm authority and applicability",
            Self::ProposedPolicy => "Proposed policy; unaccepted",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Claim {
    pub id: String,
    pub section: Section,
    pub kind: ClaimKind,
    pub text: String,
    pub rationale: String,
    pub evidence: Vec<Citation>,
    #[serde(default)]
    pub explicit_ruling: Option<Citation>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AreaDraft {
    pub instruction_path: String,
    pub claims: Vec<Claim>,
    #[serde(default)]
    pub shared_rules: Vec<Citation>,
    #[serde(default)]
    pub unresolved_decisions: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ProposedFile {
    pub path: String,
    pub expected_current: &'static str,
    pub proposed_blake3: String,
    pub proposed_content: String,
    pub unified_diff: String,
}

#[derive(Debug, Serialize)]
pub struct AreaProposal {
    pub instruction_path: String,
    pub status: &'static str,
    pub proposed_scope: Vec<String>,
    pub instruction: ProposedFile,
    pub provenance: ProposedFile,
    pub pair_blake3: String,
    pub gaps: Vec<String>,
    pub policy_correctness: &'static str,
}

pub(super) const LOOKUP_ENTRY_LIMIT: usize = 65_536;
const DIRECTORY_ENTRY_LIMIT: usize = 8_192;

fn same_path(left: &str, right: &str) -> Result<bool> {
    #[cfg(windows)]
    {
        if left.is_ascii() && right.is_ascii() {
            return Ok(left.eq_ignore_ascii_case(right));
        }
        // Match Windows ordinal file-name comparison rather than a linguistic
        // lowercase transform. Lengths are explicit UTF-16 code-unit counts.
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn CompareStringOrdinal(
                left: *const u16,
                left_len: i32,
                right: *const u16,
                right_len: i32,
                ignore_case: i32,
            ) -> i32;
        }
        ensure!(
            left.len() <= 2048 && right.len() <= 2048,
            "Path comparison exceeds lookup budget"
        );
        let left: Vec<u16> = left.encode_utf16().collect();
        let right: Vec<u16> = right.encode_utf16().collect();
        let result = unsafe {
            CompareStringOrdinal(
                left.as_ptr(),
                left.len() as i32,
                right.as_ptr(),
                right.len() as i32,
                1,
            )
        };
        ensure!(
            result != 0,
            "Windows path comparison failed: {}",
            std::io::Error::last_os_error()
        );
        Ok(result == 2)
    }
    #[cfg(not(windows))]
    {
        Ok(left == right)
    }
}

fn ancestor_of(parent: &str, child: &str) -> Result<bool> {
    let parent: Vec<_> = parent.split('/').collect();
    let child: Vec<_> = child.split('/').collect();
    if parent.len() >= child.len() {
        return Ok(false);
    }
    for (left, right) in parent.iter().zip(child) {
        if !same_path(left, right)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn ordinary_directory(metadata: &std::fs::Metadata) -> bool {
    let ordinary = metadata.is_dir() && !metadata.file_type().is_symlink();
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ordinary && metadata.file_attributes() & 0x400 == 0
    }
    #[cfg(not(windows))]
    {
        ordinary
    }
}

pub(super) fn validate_targets(
    targets: &[NewTarget],
    existing: &[String],
    evidence: &[String],
) -> Result<Vec<NewTarget>> {
    ensure!(
        targets.len() + existing.len() <= 8,
        "Existing and new instruction targets exceed eight"
    );
    let mut all: Vec<String> = Vec::new();
    for target in targets {
        paths(
            &[
                target.instruction_path.clone(),
                target.provenance_path.clone(),
            ],
            2,
        )?;
        ensure!(
            target
                .instruction_path
                .to_ascii_lowercase()
                .ends_with(".md")
                && target
                    .provenance_path
                    .to_ascii_lowercase()
                    .ends_with(".json"),
            "New area targets require Markdown instructions and separate JSON provenance"
        );
        for path in [&target.instruction_path, &target.provenance_path] {
            ensure!(
                path.split('/').count() <= 32
                    && path.split('/').all(|p| !p.eq_ignore_ascii_case(".git")
                        && !p.ends_with(['.', ' '])
                        && !p.contains('~')),
                "New output path has unsafe or ambiguous components"
            );
            for other in all.iter().chain(existing).chain(evidence) {
                ensure!(
                    !same_path(path, other)?,
                    "New output paths collide with another target or selected input"
                );
            }
            all.push(path.clone());
        }
        ensure!(
            !target.area_scope.is_empty() && target.area_scope.len() <= 16,
            "Area scope requires 1..16 patterns"
        );
        let mut scopes = BTreeSet::new();
        for pattern in &target.area_scope {
            ensure!(
                !pattern.trim().is_empty()
                    && pattern.len() <= 512
                    && !pattern.chars().any(char::is_control)
                    && !pattern.contains('\\')
                    && scopes.insert(pattern),
                "Area scope must contain bounded unique normalized patterns"
            );
            crate::services::project_policy::compile_scope(pattern).map_err(anyhow::Error::msg)?;
        }
    }
    for path in &all {
        for other in &all {
            ensure!(
                !ancestor_of(path, other)?,
                "A proposed output cannot be another output's parent directory"
            );
        }
    }
    let mut targets = targets.to_vec();
    targets.sort_by(|a, b| a.instruction_path.cmp(&b.instruction_path));
    Ok(targets)
}

pub(super) fn capture_absence(
    root: &Path,
    repo: &git2::Repository,
    tree: &git2::Tree<'_>,
    target: NewTarget,
    remaining: &mut usize,
) -> Result<AbsentTarget> {
    for path in [&target.instruction_path, &target.provenance_path] {
        engram_core::safe_join(root, path)?;
        // A missing child under an unreadable/non-directory/symlink ancestor
        // is not an available output path. Inspect all existing ancestors.
        let mut current = root.to_path_buf();
        let components: Vec<_> = path.split('/').collect();
        for (index, part) in components.iter().enumerate() {
            #[cfg(windows)]
            {
                // Enumerate only this selected path's parent. This also protects
                // untracked case aliases inside case-sensitive Windows folders.
                let mut found = None;
                let mut count = 0;
                for entry in std::fs::read_dir(&current)? {
                    count += 1;
                    ensure!(
                        count <= DIRECTORY_ENTRY_LIMIT && *remaining > 0,
                        "Current output lookup exceeds bounded directory-entry budget"
                    );
                    *remaining -= 1;
                    let entry = entry?;
                    let name = entry.file_name();
                    let name = name.to_str().context(
                        "Cannot prove output absence beside an unsupported file-name encoding",
                    )?;
                    ensure!(
                        name.len() <= 2048,
                        "Current output entry name exceeds lookup budget"
                    );
                    if same_path(name, part)? {
                        ensure!(found.is_none(), "Ambiguous Windows output path: {path}");
                        found = Some(entry.path());
                    }
                }
                let Some(found) = found else {
                    break;
                };
                current = found;
            }
            #[cfg(not(windows))]
            current.push(part);
            match std::fs::symlink_metadata(&current) {
                Ok(metadata) => ensure!(
                    index + 1 < components.len() && ordinary_directory(&metadata),
                    "New output already exists or parent is unsafe: {path}"
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) => return Err(error.into()),
            }
        }
        let mut directory = repo.find_tree(tree.id())?;
        for (index, part) in components.iter().enumerate() {
            ensure!(
                directory.len() <= DIRECTORY_ENTRY_LIMIT && directory.len() <= *remaining,
                "Historical output lookup exceeds bounded directory-entry budget"
            );
            let mut matched = None;
            for entry in directory.iter() {
                *remaining -= 1;
                let name = entry.name().context(
                    "Cannot prove historical absence beside an unsupported file-name encoding",
                )?;
                ensure!(
                    name.len() <= 2048,
                    "Historical output entry name exceeds lookup budget"
                );
                if same_path(name, part)? {
                    ensure!(
                        matched.is_none(),
                        "Ambiguous historical output path: {path}"
                    );
                    matched = Some((entry.id(), entry.kind()));
                }
            }
            let Some((oid, kind)) = matched else {
                break;
            };
            ensure!(
                index + 1 < components.len() && kind == Some(git2::ObjectType::Tree),
                "New output existed at base or historical parent was not a directory: {path}"
            );
            // Missing entry is absence. Missing object is unavailable history:
            // never reinterpret an ODB NotFound as an absent output file.
            directory = repo
                .find_tree(oid)
                .with_context(|| format!("Historical output parent unavailable: {path}"))?;
        }
    }
    Ok(AbsentTarget {
        target,
        before: "absent_at_requested_commit",
        after: "absent_on_current_disk",
    })
}

fn relative_link(from: &str, to: &str) -> String {
    let mut parent: Vec<_> = from.split('/').collect();
    parent.pop();
    let target: Vec<_> = to.split('/').collect();
    let common = parent
        .iter()
        .zip(&target)
        .take_while(|(a, b)| a == b)
        .count();
    let relative = format!(
        "{}{}",
        "../".repeat(parent.len() - common),
        target[common..].join("/")
    );
    let mut encoded = String::new();
    for byte in relative.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn checked_source(packet: &Packet, cite: &Citation) -> Result<serde_json::Value> {
    citation(packet, cite)?;
    let snapshot = cited_snapshot(packet, cite)?
        .context("Affirmative area claims need present quoted evidence, not absence")?;
    Ok(
        serde_json::json!({"citation":cite,"source_scope":manifest::scope_with_limit(snapshot, FILE_LIMIT),
        "authority":"not_established_by_citation_integrity"}),
    )
}

fn evidence_sources(
    packet: &Packet,
    draft: &AreaDraft,
) -> Result<Vec<serde_json::Value>> {
    let mut selected = BTreeMap::new();
    for cite in draft.claims.iter().flat_map(|claim| {
        claim.evidence.iter().chain(claim.explicit_ruling.iter())
    }).chain(draft.shared_rules.iter()) {
        citation(packet, cite)?;
        let snapshot = cited_snapshot(packet, cite)?
            .context("Affirmative area claims need present quoted evidence, not absence")?;
        selected.entry((cite.path.as_str(), cite.version.clone())).or_insert(snapshot);
    }
    Ok(selected.into_iter().map(|((path, version), snapshot)| {
        serde_json::json!({"path":path,"version":version,"blake3":snapshot.blake3,
            "content":snapshot.content,"source_scope":manifest::scope_with_limit(snapshot, FILE_LIMIT),
            "authority":"not_established_by_citation_integrity"})
    }).collect())
}

fn proposed_file(path: &str, content: String, limit: usize) -> Result<ProposedFile> {
    ensure!(
        content.len() <= limit,
        "Complete new area artifact exceeds {limit} bytes; reduce complete evidence, never truncate guidance"
    );
    let unified_diff = similar::TextDiff::from_lines("", &content)
        .unified_diff()
        .header("/dev/null", &format!("b/{path}"))
        .to_string();
    Ok(ProposedFile {
        path: path.into(),
        expected_current: "absent",
        proposed_blake3: blake3::hash(content.as_bytes()).to_hex().to_string(),
        proposed_content: content,
        unified_diff,
    })
}

pub(super) fn review(
    packet: &Packet,
    packet_hash: &str,
    drafts: &[AreaDraft],
) -> Result<Vec<AreaProposal>> {
    ensure!(
        drafts.len() == packet.new_instruction_targets.len(),
        "Every new area target needs exactly one draft"
    );
    let mut seen = BTreeSet::new();
    let mut proposals = Vec::new();
    for draft in drafts {
        ensure!(
            seen.insert(&draft.instruction_path),
            "Duplicate new area draft"
        );
        let declared = packet
            .new_instruction_targets
            .iter()
            .find(|t| t.target.instruction_path == draft.instruction_path)
            .context("New area draft is outside declared targets")?;
        let target = &declared.target;
        ensure!(
            !draft.claims.is_empty()
                && draft.claims.len() <= 64
                && draft.shared_rules.len() <= 16
                && draft.unresolved_decisions.len() <= 32,
            "Area draft record limits exceeded"
        );
        let patterns = target
            .area_scope
            .iter()
            .map(|p| crate::services::project_policy::compile_scope(p).map_err(anyhow::Error::msg))
            .collect::<Result<Vec<_>>>()?;
        let mut claims = draft.claims.iter().collect::<Vec<_>>();
        claims.sort_by(|a, b| a.section.cmp(&b.section).then_with(|| a.id.cmp(&b.id)));
        let mut ids = BTreeSet::new();
        let mut gaps = packet.area_evidence.as_ref().map(|area| area.gaps.clone()).unwrap_or_default();
        if !packet
            .evidence
            .iter()
            .any(|e| e.after.is_some() && patterns.iter().any(|p| p.is_match(&e.path)))
        {
            gaps.push("No current selected area file establishes the proposed scope's implementation context".into());
        }
        let mut provenance_claims = Vec::new();
        let mut grouped: BTreeMap<Section, String> = BTreeMap::new();
        for claim in claims {
            ensure!(
                !claim.id.is_empty()
                    && claim.id.len() <= 64
                    && claim
                        .id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
                    && ids.insert(&claim.id),
                "Area claim IDs must be unique bounded identifiers"
            );
            ensure!(
                !claim.text.trim().is_empty()
                    && claim.text.len() <= 4096
                    && !claim.rationale.trim().is_empty()
                    && claim.rationale.len() <= 4096
                    && claim.evidence.len() <= 16,
                "Area claims need bounded text, rationale and evidence"
            );
            let sources = claim
                .evidence
                .iter()
                .map(|cite| checked_source(packet, cite))
                .collect::<Result<Vec<_>>>()?;
            if sources.is_empty() {
                gaps.push(format!(
                    "{}: factual interpretation lacks quoted supporting evidence",
                    claim.id
                ));
            }
            if claim.kind == ClaimKind::ObservedImplementation
                && claim.section != Section::Consumer
                && !claim
                    .evidence
                    .iter()
                    .any(|c| patterns.iter().any(|p| p.is_match(&c.path)))
            {
                gaps.push(format!(
                    "{}: observation is outside the proposed area; relevance needs review",
                    claim.id
                ));
            }
            let ruling = claim
                .explicit_ruling
                .as_ref()
                .map(|cite| checked_source(packet, cite))
                .transpose()?;
            if claim.section == Section::AcceptedException {
                if ruling.is_none() {
                    gaps.push(format!("{}: no explicit ruling evidence; implementation or workflow status does not establish an accepted exception", claim.id));
                }
                if claim
                    .explicit_ruling
                    .as_ref()
                    .is_some_and(|c| c.version == Version::Before)
                    && !claim.evidence.iter().any(|c| c.version == Version::After)
                {
                    gaps.push(format!(
                        "{}: historical ruling has no current applicability evidence",
                        claim.id
                    ));
                }
            }
            provenance_claims.push(serde_json::json!({"claim":claim,"classification_origin":"drafting_agent_declaration",
                "sources":sources,"explicit_ruling":ruling,"ruling_evidence":if claim.explicit_ruling.is_some(){"citation_verified"}else{"absent"},
                "scope_relationship":if claim.section == Section::Consumer {"Consumer evidence may be outside the area; it does not broaden the declared instruction scope"} else {"Declared instruction scope is unchanged"},
                "acceptance":"not_established","interpretation_correctness":"not_established"}));
            let body = grouped.entry(claim.section.clone()).or_default();
            body.push_str(&format!(
                "\n**{}** — {}. [Provenance {}]({})\n\n",
                claim.id,
                claim.kind.label(),
                claim.id,
                relative_link(&target.instruction_path, &target.provenance_path)
            ));
            for line in claim.text.lines() {
                body.push_str(&format!("> {line}\n"));
            }
        }
        for section in [
            Section::Purpose,
            Section::Invariant,
            Section::HouseExample,
            Section::CompanionFile,
        ] {
            if !grouped.contains_key(&section) {
                gaps.push(format!("{}: no grounded draft supplied", section.heading()));
            }
        }
        if packet.area_evidence.is_some() {
            for section in [Section::EntryPoint, Section::Consumer, Section::ValidationPath] {
                if !grouped.contains_key(&section) {
                    gaps.push(format!("{}: no grounded draft supplied", section.heading()));
                }
            }
        }
        for unresolved in &draft.unresolved_decisions {
            ensure!(
                !unresolved.trim().is_empty() && unresolved.len() <= 4096,
                "Unresolved area decisions must be bounded nonblank text"
            );
            gaps.push(format!("Policy decision: {unresolved}"));
        }
        let shared = draft
            .shared_rules
            .iter()
            .map(|cite| checked_source(packet, cite))
            .collect::<Result<Vec<_>>>()?;
        let mut content = format!(
            "---\napplyTo: {}\n---\n\n# Area guidance draft\n\nDraft only. Citation checks do not establish policy acceptance or override existing guidance.\n",
            serde_json::to_string(&target.area_scope.join(","))?
        );
        if !shared.is_empty() {
            content.push_str("\n## Shared guidance — retain complete conditions and exceptions\n");
            for (index, cite) in draft.shared_rules.iter().enumerate() {
                content.push_str(&format!(
                    "\n- [Shared source {}]({}); {}scope and authority require review.\n",
                    index + 1,
                    relative_link(&target.instruction_path, &cite.path),
                    if cite.version == Version::Before {
                        "historical quote is in provenance; current file may differ; "
                    } else {
                        ""
                    }
                ));
            }
        }
        for (section, body) in &grouped {
            content.push_str(&format!("\n## {}\n{body}", section.heading()));
        }
        if !grouped.contains_key(&Section::AcceptedException) {
            content.push_str("\n## Exceptions and cited rulings — review required\n\nNo accepted exception established by this packet. This does not establish that no exceptions exist.\n");
        }
        if !gaps.is_empty() {
            content.push_str("\n## Evidence and policy gaps\n");
            for gap in &gaps {
                content.push_str(&format!("\n> {}\n", gap.replace('\n', "\n> ")));
            }
        }
        let instruction = proposed_file(&target.instruction_path, content, FILE_LIMIT)?;
        let evidence_sources = evidence_sources(packet, draft)?;
        let mut provenance_body = serde_json::json!({
                "version":1,"packet_blake3":packet_hash,"project_id":packet.project_id,"project_root_blake3":packet.project_root_blake3,
                "base_commit":packet.base_commit,"target":target,"absence_observation":declared,
                "instruction_blake3":instruction.proposed_blake3,"claims":provenance_claims,"shared_rules":shared,"gaps":gaps,
                "evidence_sources":evidence_sources,
                "policy_correctness":"not_established","policy_acceptance":"not_established","authored_files_written":0,
                "coverage":"Complete cited snapshots are included in evidence_sources, keyed by path and version. References do not broaden source scope; semantic applicability, accepted exceptions and claim correctness require independent review. No exhaustive discovery or code execution was performed."
            });
        if let Some(area) = &packet.area_evidence {
            provenance_body["area_evidence"] = serde_json::to_value(area)?;
        }
        let provenance = proposed_file(
            &target.provenance_path,
            serde_json::to_string_pretty(&provenance_body)? + "\n",
            PACKET_LIMIT,
        )?;
        let pair_blake3 = blake3::hash(&serde_json::to_vec(&serde_json::json!({
            "instruction":{"path":instruction.path,"blake3":instruction.proposed_blake3},
            "provenance":{"path":provenance.path,"blake3":provenance.proposed_blake3}
        }))?)
        .to_hex()
        .to_string();
        proposals.push(AreaProposal {
            instruction_path: target.instruction_path.clone(),
            status: if gaps.is_empty() {
                "needs_policy_review"
            } else {
                "needs_evidence"
            },
            proposed_scope: target.area_scope.clone(),
            instruction,
            provenance,
            pair_blake3,
            gaps,
            policy_correctness: "not_established",
        });
    }
    Ok(proposals)
}
