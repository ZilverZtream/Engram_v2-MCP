//! Evidence-bound instruction maintenance through the existing audit workflow.
//! Collection and exact-edit checks are deterministic. Semantic interpretation
//! belongs to the drafting/reviewing agent; this service never accepts policy
//! or writes authored files merely because citations and anchors are valid.
use anyhow::{Context, Result, ensure};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, io::Read, path::Path};

pub mod area_draft;
pub mod area_evidence;
pub mod manifest;

const FILE_LIMIT: usize = 128 * 1024;
const PACKET_LIMIT: usize = 768 * 1024;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MaintenanceRequest {
    /// Assemble indexed area files, consumers and applicable shared guidance
    /// before drafting. Available through pre_push_audit; explicit-path offline
    /// maintenance remains unchanged when this is omitted.
    #[serde(default)]
    pub area_context: Option<area_evidence::AreaSelection>,
    /// Complete authored guidance to review. Existing and new targets together
    /// are limited to eight. New-only drafting may omit existing instructions.
    #[serde(default)]
    pub instruction_paths: Vec<String>,
    /// New area guidance and its separate provenance artifact; both must be
    /// absent at the requested commit and on current disk. Proposals only.
    #[serde(default)]
    pub new_instruction_targets: Vec<area_draft::NewTarget>,
    /// Complete code/shared guidance/decision evidence, 1..16 explicit paths.
    /// Optional seeds when area_context assembles evidence automatically.
    /// Selection is not proof that every dependency was discovered.
    #[serde(default)]
    pub evidence_paths: Vec<String>,
    /// Exact full commit ID for before evidence. After evidence is current disk.
    pub base_commit: String,
    /// Omit to collect evidence. Submit the drafting agent's grounded proposal
    /// to check current dependency hashes, exact anchors and quoted evidence.
    #[serde(default)]
    pub draft: Option<MaintenanceDraft>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MaintenanceDraft {
    pub expected_packet_blake3: String,
    /// One explicit disposition per complete instruction in the packet.
    #[serde(default)]
    pub decisions: Vec<Decision>,
    #[serde(default)]
    pub new_area_files: Vec<area_draft::AreaDraft>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Change,
    NoChange,
    Conflict,
    NeedsEvidence,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    pub instruction_path: String,
    pub outcome: Outcome,
    pub rationale: String,
    /// Actual before/after quotes supporting the semantic disposition.
    pub evidence: Vec<Citation>,
    #[serde(default)]
    pub edits: Vec<Edit>,
    /// Exact accepted exceptions/conditions the drafting agent declares must
    /// survive. Engram verifies these fragments, not completeness of the list.
    #[serde(default)]
    pub preserved_fragments: Vec<String>,
    #[serde(default)]
    pub unresolved_decisions: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Edit {
    /// Unique exact anchor in the original complete instruction.
    pub old_text: String,
    pub new_text: String,
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Version {
    Before,
    After,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Citation {
    pub path: String,
    pub version: Version,
    /// Hash of the cited snapshot, null only for proven absence.
    pub blake3: Option<String>,
    /// Exact nonempty quote, or null to cite proven absence of that file.
    pub quote: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Snapshot {
    pub blake3: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Instruction {
    pub path: String,
    pub current: Snapshot,
    /// Authored guidance at the exact requested base, never today's substitute.
    pub before: Option<Snapshot>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Evidence {
    pub path: String,
    pub before: Option<Snapshot>,
    pub after: Option<Snapshot>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Packet {
    pub version: u32,
    pub project_id: String,
    pub project_root_blake3: String,
    pub base_commit: String,
    pub instructions: Vec<Instruction>,
    pub new_instruction_targets: Vec<area_draft::AbsentTarget>,
    pub evidence: Vec<Evidence>,
    pub references: Vec<Reference>,
    pub instruction_manifest: manifest::Manifest,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub area_evidence: Option<area_evidence::AreaEvidence>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Reference {
    pub instruction_path: String,
    pub target: String,
    pub status: &'static str,
}

#[derive(Debug, Serialize)]
pub struct Proposal {
    pub instruction_path: String,
    pub outcome: Outcome,
    pub rationale: String,
    pub expected_source_blake3: String,
    pub proposed_blake3: String,
    pub proposed_content: String,
    pub unified_diff: String,
    pub evidence: Vec<Citation>,
    pub preserved_fragments: Vec<String>,
    pub unresolved_decisions: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub status: &'static str,
    pub packet_blake3: String,
    pub packet: Packet,
    pub proposals: Vec<Proposal>,
    pub new_area_proposals: Vec<area_draft::AreaProposal>,
    pub proposal_blake3: Option<String>,
    pub coverage: &'static str,
    pub review_requirements: &'static str,
    pub authored_files_written: usize,
}

fn paths(input: &[String], limit: usize) -> Result<Vec<String>> {
    ensure!(
        !input.is_empty() && input.len() <= limit,
        "Expected 1..{limit} explicit paths"
    );
    let mut unique = BTreeSet::new();
    for path in input {
        ensure!(
            path.len() <= 512
                && !path.contains('\\')
                && !path.contains(':')
                && path
                    .split('/')
                    .all(|part| !part.is_empty() && part != "." && part != "..")
                && !path.chars().any(char::is_control),
            "Use normalized project-relative paths"
        );
        ensure!(unique.insert(path.clone()), "Duplicate maintenance path");
    }
    Ok(unique.into_iter().collect())
}

fn snapshot(bytes: Vec<u8>, remaining: &mut usize) -> Result<Snapshot> {
    ensure!(
        bytes.len() <= FILE_LIMIT && bytes.len() <= *remaining,
        "Complete maintenance evidence exceeds byte budget; select a smaller complete dependency set, never truncate guidance"
    );
    *remaining -= bytes.len();
    Ok(Snapshot {
        blake3: blake3::hash(&bytes).to_hex().to_string(),
        content: String::from_utf8(bytes)
            .context("Maintenance evidence must be UTF-8; no lossy conversion")?,
    })
}

fn disk(root: &Path, relative: &str, remaining: &mut usize) -> Result<Option<Snapshot>> {
    let path = engram_core::safe_join(root, relative)?;
    let before_open = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    ensure!(
        before_open.is_file() && !before_open.file_type().is_symlink(),
        "Maintenance input must be a regular file: {relative}"
    );
    // Enforce the existing no-follow open contract at the syscall boundary,
    // rather than relying only on safe_join's earlier component inspection.
    let file = engram_core::safe_open_read(root, relative)?;
    let info = file.metadata()?;
    ensure!(
        info.is_file(),
        "Maintenance input must be a regular file: {relative}"
    );
    let limit = FILE_LIMIT.min(*remaining);
    ensure!(
        info.len() <= limit as u64,
        "Complete maintenance input exceeds byte budget: {relative}"
    );
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    snapshot(bytes, remaining).map(Some)
}

fn committed(
    repo: &git2::Repository,
    tree: &git2::Tree<'_>,
    path: &str,
    remaining: &mut usize,
) -> Result<Option<Snapshot>> {
    // Missing entries establish literal Git-path absence. Missing referenced
    // objects establish unavailable history, even when libgit2 reports NotFound.
    let parts = path.split('/').collect::<Vec<_>>();
    let mut directory = repo.find_tree(tree.id())?;
    let mut selected = None;
    for (index, part) in parts.iter().enumerate() {
        let Some(entry) = directory.get_name(part) else {
            return Ok(None);
        };
        let (id, mode, kind) = (entry.id(), entry.filemode(), entry.kind());
        drop(entry);
        if index + 1 == parts.len() {
            selected = Some((id, mode));
            break;
        }
        ensure!(
            kind == Some(git2::ObjectType::Tree),
            "Historical input parent is not a directory: {path}"
        );
        directory = repo
            .find_tree(id)
            .with_context(|| format!("Historical input parent unavailable: {path}"))?;
    }
    let (id, mode) = selected.context("Empty historical input path")?;
    ensure!(
        mode == 0o100644 || mode == 0o100755,
        "Committed maintenance evidence must be a regular file: {path}"
    );
    let (bytes, kind) = repo.odb()?.read_header(id)?;
    ensure!(
        kind == git2::ObjectType::Blob && bytes <= FILE_LIMIT.min(*remaining),
        "Complete committed evidence exceeds byte budget or is not a blob: {path}"
    );
    let blob = repo.find_blob(id)?;
    ensure!(
        blob.size() <= FILE_LIMIT.min(*remaining),
        "Complete committed evidence exceeds byte budget: {path}"
    );
    snapshot(blob.content().to_vec(), remaining).map(Some)
}

/// Pure request validation: callers can reject malformed observation requests
/// before starting an update, without reading or writing project files.
pub fn validate_observation_request(req: &MaintenanceRequest) -> Result<()> {
    ensure!(req.area_context.is_none(), "Collect area evidence through pre_push_audit");
    ensure!(
        req.new_instruction_targets.is_empty(),
        "Collect new area drafts through pre_push_audit; this operation observes existing instructions"
    );
    ensure!(
        req.draft.is_none(),
        "Submit instruction drafts through pre_push_audit; this operation only observes references"
    );
    paths(&req.instruction_paths, 8)?;
    paths(&req.evidence_paths, 16)?;
    ensure!(
        req.base_commit.len() == 40 && req.base_commit.bytes().all(|b| b.is_ascii_hexdigit()),
        "base_commit must be an exact full commit ID"
    );
    Ok(())
}

pub fn capture(root: &Path, project_id: &str, req: &MaintenanceRequest) -> Result<Packet> {
    ensure!(req.area_context.is_none(), "Indexed area evidence is collected through pre_push_audit");
    capture_with_area(root, project_id, req, None)
}

fn capture_with_area(root: &Path, project_id: &str, req: &MaintenanceRequest,
    area: Option<&area_evidence::AreaEvidence>) -> Result<Packet> {
    let instruction_paths =
        if req.instruction_paths.is_empty() && !req.new_instruction_targets.is_empty() {
            Vec::new()
        } else {
            paths(&req.instruction_paths, 8)?
        };
    let evidence_paths = if req.evidence_paths.is_empty() && area.is_some() {
        Vec::new()
    } else { paths(&req.evidence_paths, 16)? };
    let targets = area_draft::validate_targets(
        &req.new_instruction_targets,
        &instruction_paths,
        &evidence_paths,
    )?;
    ensure!(
        req.base_commit.len() == 40 && req.base_commit.bytes().all(|b| b.is_ascii_hexdigit()),
        "base_commit must be an exact full commit ID"
    );
    let root = std::fs::canonicalize(root)?;
    let repo =
        git2::Repository::open(&root).context("Maintenance needs this project's Git repository")?;
    let tree = repo
        .find_commit(git2::Oid::from_str(&req.base_commit)?)?
        .tree()?;
    let mut remaining = PACKET_LIMIT;
    let mut lookup_remaining = area_draft::LOOKUP_ENTRY_LIMIT;
    let new_instruction_targets = targets
        .into_iter()
        .map(|target| {
            area_draft::capture_absence(&root, &repo, &tree, target, &mut lookup_remaining)
        })
        .collect::<Result<Vec<_>>>()?;
    let mut instructions = Vec::new();
    for path in instruction_paths {
        let current = disk(&root, &path, &mut remaining)?
            .with_context(|| format!("Complete authored instruction unavailable: {path}"))?;
        instructions.push(Instruction {
            path,
            current,
            before: None,
        });
    }
    let mut evidence = Vec::new();
    for path in evidence_paths {
        // Validate even absent worktree paths before consulting the Git tree.
        engram_core::safe_join(&root, &path)?;
        // Inspect object metadata before explicit body materialization.
        // Native ODB decompression is not a hard process-RSS guarantee.
        let before = committed(&repo, &tree, &path, &mut remaining)?;
        let after = disk(&root, &path, &mut remaining)?;
        ensure!(
            before.is_some() || after.is_some(),
            "Evidence path absent both before and after: {path}"
        );
        evidence.push(Evidence {
            path,
            before,
            after,
        });
    }
    // Explicit complete inputs take precedence over automatic dependencies.
    for instruction in &mut instructions {
        instruction.before = committed(&repo, &tree, &instruction.path, &mut remaining)?;
    }
    let shared_roots = area.map(|area| area.files.iter()
        .filter(|file| file.roles.contains("shared_guidance"))
        .map(|file| file.path.clone()).collect::<Vec<_>>()).unwrap_or_default();
    let instruction_manifest = manifest::capture(
        &root,
        &repo,
        &tree,
        &req.base_commit,
        &instructions,
        &evidence,
        &shared_roots,
        area.map(|area| area.excluded_paths.as_slice()).unwrap_or_default(),
        &mut remaining,
    )?;
    let references = references(&instructions, &instruction_manifest);
    let mut area_evidence = area.cloned();
    if let Some(area) = &mut area_evidence {
        area_evidence::bind(area, &evidence, &instructions);
    }
    Ok(Packet {
        version: if area.is_some() { 5 } else { 4 },
        project_id: project_id.into(),
        project_root_blake3: blake3::hash(root.to_string_lossy().as_bytes())
            .to_hex()
            .to_string(),
        base_commit: req.base_commit.to_ascii_lowercase(),
        instructions,
        new_instruction_targets,
        evidence,
        references,
        instruction_manifest,
        area_evidence,
    })
}

fn references(instructions: &[Instruction], manifest: &manifest::Manifest) -> Vec<Reference> {
    // Compatibility view of the same bounded graph, never a second parser
    // that can fail before the graph's explicit omission report is returned.
    manifest
        .edges
        .iter()
        .filter(|edge| {
            edge.version == Version::After
                && instructions.iter().any(|i| i.path == edge.source_path)
        })
        .map(|edge| Reference {
            instruction_path: edge.source_path.clone(),
            target: edge.target.clone(),
            status: match edge.status {
                "complete" => "selected_complete_current_content",
                "content_complete_fragment_unverified" => {
                    "selected_content_symbol_anchor_unverified"
                }
                "absent" => "unresolved_reference_missing",
                status => status,
            },
        })
        .collect()
}

pub fn packet_hash(packet: &Packet) -> Result<String> {
    Ok(blake3::hash(&serde_json::to_vec(packet)?)
        .to_hex()
        .to_string())
}

fn cited_snapshot<'a>(packet: &'a Packet, cite: &Citation) -> Result<Option<&'a Snapshot>> {
    let source = if let Some(item) = packet.evidence.iter().find(|e| e.path == cite.path) {
        match cite.version {
            Version::Before => item.before.as_ref(),
            Version::After => item.after.as_ref(),
        }
    } else if let Some(item) = packet.instructions.iter().find(|i| i.path == cite.path) {
        match cite.version {
            Version::Before => item.before.as_ref(),
            Version::After => Some(&item.current),
        }
    } else if let Some(item) = packet
        .instruction_manifest
        .dependencies
        .iter()
        .find(|e| e.path == cite.path)
    {
        let observation = match cite.version {
            Version::Before => &item.before,
            Version::After => &item.after,
        };
        ensure!(
            matches!(observation.status, "complete" | "absent"),
            "Unavailable dependency is not proof of absence"
        );
        observation.snapshot.as_ref()
    } else {
        anyhow::bail!("Proposal cites evidence outside the complete packet")
    };
    Ok(source)
}

fn citation(packet: &Packet, cite: &Citation) -> Result<()> {
    let source = cited_snapshot(packet, cite)?;
    ensure!(
        source.map(|s| &s.blake3) == cite.blake3.as_ref(),
        "Citation snapshot hash mismatch"
    );
    match (source, &cite.quote) {
        (Some(source), Some(quote)) => ensure!(
            !quote.trim().is_empty() && quote.len() <= FILE_LIMIT && source.content.contains(quote),
            "Evidence quote does not match its exact before/after source"
        ),
        (None, None) => {}
        _ => anyhow::bail!(
            "An absence citation must match an absent file; present evidence needs an exact quote"
        ),
    }
    Ok(())
}

fn validate(packet: &Packet, draft: &MaintenanceDraft) -> Result<Vec<Proposal>> {
    ensure!(
        serde_json::to_vec(draft)?.len() <= 1024 * 1024,
        "Maintenance draft exceeds byte budget"
    );
    ensure!(
        draft.expected_packet_blake3 == packet_hash(packet)?,
        "Stale maintenance proposal: instruction, relevant evidence or project identity changed; recollect and review"
    );
    ensure!(
        draft.decisions.len() == packet.instructions.len(),
        "Every instruction needs exactly one explicit disposition"
    );
    let mut seen = BTreeSet::new();
    let mut proposals = Vec::new();
    for decision in &draft.decisions {
        ensure!(
            seen.insert(&decision.instruction_path),
            "Duplicate instruction disposition"
        );
        let input = packet
            .instructions
            .iter()
            .find(|i| i.path == decision.instruction_path)
            .context("Proposal instruction is outside the complete packet")?;
        ensure!(
            !decision.rationale.trim().is_empty() && decision.rationale.len() <= 4096,
            "Each disposition needs a bounded semantic rationale"
        );
        ensure!(
            decision.evidence.len() <= 64
                && decision.edits.len() <= 16
                && decision.preserved_fragments.len() <= 64
                && decision.unresolved_decisions.len() <= 32,
            "Maintenance proposal record limit exceeded"
        );
        if decision.outcome != Outcome::NeedsEvidence {
            ensure!(
                decision
                    .evidence
                    .iter()
                    .any(|c| c.version == Version::Before)
                    && decision
                        .evidence
                        .iter()
                        .any(|c| c.version == Version::After),
                "Disposition requires exact before and after evidence"
            );
        }
        for cite in &decision.evidence {
            citation(packet, cite)?;
        }
        ensure!(
            (decision.outcome == Outcome::Change) != decision.edits.is_empty(),
            "Only change proposals may have edits; change needs at least one edit"
        );
        let original = &input.current.content;
        let mut ranges = Vec::new();
        for edit in &decision.edits {
            ensure!(
                !edit.old_text.is_empty()
                    && !edit.reason.trim().is_empty()
                    && edit.reason.len() <= 4096,
                "Every edit needs an exact nonempty original anchor and semantic reason"
            );
            let start = original
                .find(&edit.old_text)
                .context("Edit anchor must occur exactly once in original guidance")?;
            // str::match_indices skips overlapping occurrences ("aa" in
            // "aaa"). A second search one character after the first start
            // detects ambiguity without quadratic byte-window comparisons.
            let next = start
                + original[start..]
                    .chars()
                    .next()
                    .context("Empty edit anchor")?
                    .len_utf8();
            ensure!(
                !original[next..].contains(&edit.old_text),
                "Edit anchor must occur exactly once in original guidance"
            );
            ranges.push((start, start + edit.old_text.len(), &edit.new_text));
        }
        ranges.sort_by_key(|r| r.0);
        ensure!(
            ranges.windows(2).all(|pair| pair[0].1 <= pair[1].0),
            "Instruction edits overlap"
        );
        let size = ranges
            .iter()
            .try_fold(original.len(), |size, (start, end, new)| {
                size.checked_sub(end - start)
                    .and_then(|n| n.checked_add(new.len()))
            })
            .context("Proposed instruction size overflow")?;
        ensure!(
            size <= FILE_LIMIT,
            "Complete proposed instruction exceeds byte budget"
        );
        let mut proposed = original.clone();
        for (start, end, new) in ranges.into_iter().rev() {
            proposed.replace_range(start..end, new);
        }
        for fragment in &decision.preserved_fragments {
            ensure!(
                !fragment.trim().is_empty()
                    && original.contains(fragment)
                    && proposed.contains(fragment),
                "Declared accepted exception/condition was removed or changed"
            );
        }
        ensure!(
            decision.outcome != Outcome::Change || proposed != *original,
            "Change disposition must actually change guidance; use an explained no_change"
        );
        let unified_diff = similar::TextDiff::from_lines(original, &proposed)
            .unified_diff()
            .header(&format!("a/{}", input.path), &format!("b/{}", input.path))
            .to_string();
        proposals.push(Proposal {
            instruction_path: input.path.clone(),
            outcome: decision.outcome.clone(),
            rationale: decision.rationale.clone(),
            expected_source_blake3: input.current.blake3.clone(),
            proposed_blake3: blake3::hash(proposed.as_bytes()).to_hex().to_string(),
            proposed_content: proposed,
            unified_diff,
            evidence: decision.evidence.clone(),
            preserved_fragments: decision.preserved_fragments.clone(),
            unresolved_decisions: decision.unresolved_decisions.clone(),
        });
    }
    Ok(proposals)
}

pub fn review(root: &Path, project_id: &str, req: &MaintenanceRequest) -> Result<Report> {
    ensure!(req.area_context.is_none(), "Indexed area evidence is collected through pre_push_audit");
    review_with_area(root, project_id, req, None)
}

pub(crate) fn review_with_area(root: &Path, project_id: &str, req: &MaintenanceRequest,
    area: Option<&area_evidence::AreaEvidence>) -> Result<Report> {
    let packet = capture_with_area(root, project_id, req, area)?;
    let packet_blake3 = packet_hash(&packet)?;
    let proposals = req
        .draft
        .as_ref()
        .map(|draft| validate(&packet, draft))
        .transpose()?
        .unwrap_or_default();
    let new_area_proposals = req
        .draft
        .as_ref()
        .map(|draft| area_draft::review(&packet, &packet_blake3, &draft.new_area_files))
        .transpose()?
        .unwrap_or_default();
    let proposal_blake3 = req
        .draft
        .as_ref()
        .map(|draft| {
            serde_json::to_vec(draft).map(|bytes| blake3::hash(&bytes).to_hex().to_string())
        })
        .transpose()?;
    // Recheck the complete bounded dependency set before returning either pack
    // or validated proposal. This is an observation, never a filesystem lock
    // or authorization to apply after a later human edit.
    ensure!(
        packet == capture_with_area(root, project_id, req, area)?,
        "Maintenance inputs changed during collection; retry"
    );
    let report = Report {
        status: if req.draft.is_some() {
            "proposal_requires_semantic_review"
        } else {
            "draft_required"
        },
        packet_blake3,
        packet,
        proposals,
        new_area_proposals,
        proposal_blake3,
        coverage: "Complete explicit inputs plus bounded linked dependencies, with exact requested-commit guidance and evidence. The instruction_manifest names unresolved, unsupported and budget-omitted references; fragments are not verified symbols. Move candidates cover only collected exact-content paths. Per-instruction impact is separate from strict whole-packet draft revalidation. No exhaustive repository discovery, semantic policy acceptance or historical execution is claimed.",
        review_requirements: "Draft or independently review each disposition against the complete evidence. Preserve all accepted conditions, exceptions, scope and shared references; preserved_fragments is a declaration, not proof of completeness. Explain no_change. Conflicting implementation may be a code defect, never automatic permission to weaken valid policy. New mandates or changed exceptions need an explicit accepted decision. Resolve all missing evidence and policy intent before human approval. No authored file or policy ownership is changed. Revalidate this exact packet and proposal before applying through the existing reviewed editing workflow; acceptance/application remains a separate operation.",
        authored_files_written: 0,
    };
    ensure!(
        serde_json::to_vec(&report)?.len() <= 4 * 1024 * 1024,
        "Complete maintenance packet/proposal/diff exceeds response budget; no partial review returned"
    );
    Ok(report)
}
