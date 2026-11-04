//! Refresh generated guidance only when a receipt proves its current bytes.
//!
//! A recognizable filename or generated header is not ownership evidence.
//! Directory-bound journals reconcile only verified owned partial operations.
//! Legacy/unverifiable recovery evidence remains preserved for review.
use engram_core::registry::Registry;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::Read,
    path::Path,
};

const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
mod guarded;
mod recovery;
#[cfg(all(test, windows))]
mod race_tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event { Observed, Guarded, Prepared, Retired, Installed, Removed, BeforeDependent, BeforeRemoval }

fn event(event: Event, relative: &str, root: &Path) -> anyhow::Result<()> {
    #[cfg(test)]
    return TEST_HOOK.with(|hook| match hook.borrow_mut().as_mut() {
        Some(hook) => hook(event, relative, root), None => Ok(()),
    });
    #[cfg(not(test))]
    { let _ = (event, relative, root); Ok(()) }
}

#[cfg(test)]
type Hook = Box<dyn FnMut(Event, &str, &Path) -> anyhow::Result<()>>;
#[cfg(test)]
thread_local! { static TEST_HOOK: std::cell::RefCell<Option<Hook>> = const { std::cell::RefCell::new(None) }; }
#[cfg(all(test, windows))]
struct HookOwner;
#[cfg(all(test, windows))]
impl HookOwner {
    fn install(hook: Hook) -> Self { TEST_HOOK.with(|slot| *slot.borrow_mut() = Some(hook)); Self }
}
#[cfg(all(test, windows))]
impl Drop for HookOwner {
    fn drop(&mut self) { TEST_HOOK.with(|slot| *slot.borrow_mut() = None); }
}

#[derive(Debug, Default)]
pub(crate) struct Publication {
    pub written: Vec<String>,
    pub notes: Vec<String>,
    pub outcomes: BTreeMap<String, &'static str>,
    pub observed_hashes: BTreeMap<String, String>,
}

#[derive(Debug, thiserror::Error)]
#[error("Generated artifact publication stopped: {error}")]
pub(crate) struct PublicationFailure {
    pub report: Publication,
    #[source]
    error: anyhow::Error,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    version: u32,
    root: String,
    owner: String,
    files: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending: Option<Pending>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    binding: Option<recovery::Binding>,
    operation: String,
    relative: String,
    expected_hash: Option<String>,
    expected_identity: Option<String>,
    proposed_hash: Option<String>,
    staged_identity: Option<String>,
    staged: Option<String>,
    recovery: Option<String>,
    phase: String,
}

fn persist_receipt(registry: &Registry, project: &str, key: &str, receipt: &Receipt) -> anyhow::Result<()> {
    let bytes = serde_json::to_string(receipt)?;
    anyhow::ensure!(bytes.len() <= 256 * 1024, "Generated-artifact receipt exceeds its journal budget");
    registry.set_meta(project, key, &bytes)
}

fn note_pending(registry: &Registry, project: &str, owner: &str, report: &mut Publication) {
    if let Ok(Some(raw)) = registry.get_meta(project, &format!("generated_artifacts:{owner}")) {
        if raw.len() <= 256 * 1024 {
            if let Ok(receipt) = serde_json::from_str::<Receipt>(&raw) {
                if let Some(pending) = receipt.pending {
                    report.notes.push(format!("RETAINED publication recovery for {}: phase {}, owned stage {:?}, preimage {:?}. The receipt records expected identities and hashes. Preserve these exact paths until this pending operation is inspected and reconciled; complete proposed contents remain returned.", pending.relative, pending.phase, pending.staged, pending.recovery));
                }
            }
        }
    }
}

fn current_hash(root: &Path, relative: &str) -> anyhow::Result<Option<String>> {
    let path = engram_core::safe_join(root, relative)?;
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "not a regular generated artifact"
    );
    anyhow::ensure!(
        metadata.len() <= MAX_FILE_BYTES,
        "artifact exceeds ownership verification limit"
    );
    let mut bytes = Vec::new();
    engram_core::safe_open_read(root, relative)?
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_FILE_BYTES,
        "artifact exceeds ownership verification limit"
    );
    Ok(Some(blake3::hash(&bytes).to_hex().to_string()))
}

/// Caller holds the project's update lock, shared with other guidance writers.
/// Explicit user-requested root-policy replacement has its own backup contract;
/// this function handles automatically refreshed files and diverted drafts.
pub(crate) fn publish(
    registry: &Registry,
    project: &str,
    root: &Path,
    owner: &str,
    artifacts: &[(String, String)],
) -> anyhow::Result<Publication> {
    publish_bundle(
        registry,
        project,
        root,
        owner,
        artifacts,
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
}

/// Integration settings/root files are create-only even after our first install.
pub(crate) fn publish_create_only(
    registry: &Registry, project: &str, root: &Path, owner: &str, artifacts: &[(String, String)],
) -> anyhow::Result<Publication> {
    let mut report = Publication::default();
    let create_only = artifacts.iter().map(|(path, _)| path.as_str()).collect::<std::collections::BTreeSet<_>>();
    match publish_inner(registry, project, root, owner, artifacts, &BTreeMap::new(), &BTreeMap::new(), &create_only, &mut report) {
        Ok(()) => Ok(report),
        Err(error) => { note_pending(registry, project, owner, &mut report); Err(PublicationFailure { report, error }.into()) },
    }
}

/// Root instructions may depend on complete companion policy. An explicit
/// replacement authorizes only the preimage hash read and backed up by caller.
pub(crate) fn publish_bundle(
    registry: &Registry,
    project: &str,
    root: &Path,
    owner: &str,
    artifacts: &[(String, String)],
    dependencies: &BTreeMap<String, Vec<String>>,
    authorized_preimages: &BTreeMap<String, String>,
) -> anyhow::Result<Publication> {
    let mut report = Publication::default();
    match publish_inner(
        registry,
        project,
        root,
        owner,
        artifacts,
        dependencies,
        authorized_preimages,
        &std::collections::BTreeSet::new(),
        &mut report,
    ) {
        Ok(()) => Ok(report),
        Err(error) => { note_pending(registry, project, owner, &mut report); Err(PublicationFailure { report, error }.into()) },
    }
}

/// Plan the complete bundle before the first file write. Missing companions
/// keep their existing conflict handling; cycles cannot produce partial files.
fn publication_order<'a>(
    desired: &BTreeMap<&'a str, &'a str>,
    dependencies: &BTreeMap<String, Vec<String>>,
) -> anyhow::Result<Vec<&'a str>> {
    fn visit<'a>(
        path: &'a str,
        desired: &BTreeMap<&'a str, &'a str>,
        dependencies: &BTreeMap<String, Vec<String>>,
        active: &mut std::collections::BTreeSet<&'a str>,
        complete: &mut std::collections::BTreeSet<&'a str>,
        order: &mut Vec<&'a str>,
    ) -> anyhow::Result<()> {
        if complete.contains(path) {
            return Ok(());
        }
        anyhow::ensure!(
            active.insert(path),
            "Generated artifact dependency cycle; no files published"
        );
        if let Some(required) = dependencies.get(path) {
            for dependency in required {
                if let Some((&key, _)) = desired.get_key_value(dependency.as_str()) {
                    visit(key, desired, dependencies, active, complete, order)?;
                }
            }
        }
        active.remove(path);
        complete.insert(path);
        order.push(path);
        Ok(())
    }
    let mut order = Vec::with_capacity(desired.len());
    let mut active = std::collections::BTreeSet::new();
    let mut complete = std::collections::BTreeSet::new();
    for path in desired.keys() {
        visit(
            path,
            desired,
            dependencies,
            &mut active,
            &mut complete,
            &mut order,
        )?;
    }
    Ok(order)
}

fn publish_inner(
    registry: &Registry,
    project: &str,
    root: &Path,
    owner: &str,
    artifacts: &[(String, String)],
    dependencies: &BTreeMap<String, Vec<String>>,
    authorized_preimages: &BTreeMap<String, String>,
    create_only: &std::collections::BTreeSet<&str>,
    result: &mut Publication,
) -> anyhow::Result<()> {
    anyhow::ensure!(artifacts.len() <= 512, "too many generated artifacts");
    // This authority must precede the receipt and every hash observation. A
    // later guard would merely protect a replacement root selected by a race.
    let authority = match guarded::Root::open(root) {
        Ok(authority) => authority,
        Err(error) => {
            for (relative, _) in artifacts { result.outcomes.insert(relative.clone(), "preserved_unverifiable"); }
            result.notes.push(format!("PRESERVED publication: mandatory project-root authority unavailable ({error}); complete proposals returned."));
            return Err(error);
        }
    };
    let canonical_root = authority.path.clone();
    let identity = canonical_root.to_string_lossy().into_owned();
    let key = format!("generated_artifacts:{owner}");
    let fresh_receipt = || Receipt {
        version: 1,
        root: identity.clone(),
        owner: owner.into(),
        files: BTreeMap::new(),
        pending: None,
    };
    let mut receipt_unavailable = false;
    let mut receipt = match registry.get_meta(project, &key)? {
        Some(raw) => {
            let parsed = (raw.len() <= 256 * 1024)
                .then(|| serde_json::from_str::<Receipt>(&raw).ok())
                .flatten()
                .filter(|receipt| {
                    receipt.version == 1
                        && receipt.root == identity
                        && receipt.owner == owner
                        && receipt.files.len() <= 512
                });
            match parsed {
                Some(receipt) => receipt,
                None => {
                    receipt_unavailable = true;
                    result.notes.push("Generated-artifact receipt is unreadable or belongs to another root/version; existing files are preserved without assuming ownership. Proposed contents remain available for review.".into());
                    fresh_receipt()
                }
            }
        }
        None => fresh_receipt(),
    };
    let desired: BTreeMap<_, _> = artifacts
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect();
    anyhow::ensure!(
        desired.len() == artifacts.len(),
        "duplicate generated artifact path"
    );
    for relative in desired.keys() {
        result.outcomes.insert((*relative).into(), "not_attempted");
    }
    let order = publication_order(&desired, dependencies)?;
    if receipt_unavailable {
        // An unreadable/moved receipt may contain interrupted-operation
        // evidence. Do not overwrite that journal with a fresh empty receipt.
        for relative in desired.keys() { result.outcomes.insert((*relative).into(), "preserved_unverifiable"); }
        return Ok(());
    }
    let proposal_bundle_hash = recovery::bundle_hash(&desired, dependencies, authorized_preimages, create_only)?;
    if receipt.pending.is_some() && !recovery::reconcile(
        registry, project, &key, &authority, &mut receipt, &proposal_bundle_hash,
        &desired, authorized_preimages, result,
    )? {
        let pending = receipt.pending.as_ref().expect("unreconciled journal");
        for relative in desired.keys() { result.outcomes.insert((*relative).into(), "preserved_unverifiable"); }
        result.notes.push(format!("PRESERVED pending {} for {} (phase {}): ownership/bundle proof is unavailable or changed; owned stage {:?}, preimage recovery {:?}. Preserve this journal and its exact files; complete proposals remain returned.",
            pending.operation, pending.relative, pending.phase, pending.staged, pending.recovery));
        return Ok(());
    }
    // Each dependency handle also retains its checked directory ancestry.
    // Bound the number of retained handle groups rather than reopening cached
    // successes without protection before a dependent root is installed.
    let mut held = BTreeMap::<String, guarded::HeldFile>::new();
    for relative in order {
        let (relative, text) = desired
            .get_key_value(relative)
            .expect("ordered requested artifact");
        anyhow::ensure!(
            text.len() as u64 <= MAX_FILE_BYTES,
            "generated artifact exceeds write limit"
        );
        if let Some(required) = dependencies.get(*relative) {
            event(Event::BeforeDependent, relative, &canonical_root)?;
            let unresolved = required
                .iter()
                .filter(|dependency| {
                    let wanted = desired
                        .get(dependency.as_str())
                        .map(|text| blake3::hash(text.as_bytes()).to_hex().to_string());
                    let trusted = matches!(
                        result.outcomes.get(dependency.as_str()).copied(),
                        Some("written" | "unchanged_receipt_owned")
                    );
                    !trusted || !held.contains_key(dependency.as_str())
                        || wanted.as_ref() != result.observed_hashes.get(dependency.as_str())
                })
                .cloned()
                .collect::<Vec<_>>();
            if !unresolved.is_empty() {
                result
                    .outcomes
                    .insert((*relative).into(), "preserved_dependency_conflict");
                result.notes.push(format!("PRESERVED {relative}: complete companion dependencies were not installed at their proposed hashes: {}; root proposal is returned without replacing current policy.", unresolved.join(", ")));
                continue;
            }
        }
        // Pin the containing scope before its first hash observation too. Root
        // authority alone cannot protect a nested directory swapped afterward.
        let parent = match guarded::Parent::open(&authority, relative, true) {
            Ok(parent) => parent,
            Err(error) => {
                result.outcomes.insert((*relative).into(), "preserved_unverifiable");
                result.notes.push(format!("PRESERVED {relative}: checked parent authority unavailable ({error}); complete proposal returned."));
                return Err(error);
            }
        };
        let current = match current_hash(&canonical_root, relative) {
            Ok(current) => current,
            Err(error) => {
                result
                    .outcomes
                    .insert((*relative).into(), "preserved_unverifiable");
                result.notes.push(format!("PRESERVED {relative}: ownership could not be verified ({error}); proposed content is returned for review."));
                continue;
            }
        };
        event(Event::Observed, relative, &canonical_root)?;
        let hash = blake3::hash(text.as_bytes()).to_hex().to_string();
        if let Some(current) = current.as_ref() {
            result
                .observed_hashes
                .insert((*relative).into(), current.clone());
            if create_only.contains(*relative) {
                result.outcomes.insert((*relative).into(), "preserved_conflict");
                result.notes.push(format!("PRESERVED {relative}: create-only artifact already exists; merge the returned proposal manually."));
                continue;
            }
            if receipt.files.get(*relative) != Some(current)
                && authorized_preimages.get(*relative) != Some(current)
            {
                result
                    .outcomes
                    .insert((*relative).into(), "preserved_conflict");
                result.notes.push(format!("PRESERVED {relative}: unreceipted or edited outside Engram; proposed content is returned for review."));
                continue;
            }
        }
        let protected = (|| -> anyhow::Result<_> {
            let existing = parent.existing(relative)?;
            Ok((parent, existing))
        })();
        let (parent, mut existing) = match protected {
            Ok(protected) => protected,
            Err(error) => {
                result.outcomes.insert((*relative).into(), "preserved_unverifiable");
                result.notes.push(format!("PRESERVED {relative}: mandatory ownership guard unavailable ({error}); complete proposal returned."));
                return Err(error);
            }
        };
        if existing.as_ref().map(|file| &file.hash) != current.as_ref() {
            result.outcomes.insert((*relative).into(), "preserved_changed");
            if let Some(file) = &existing { result.observed_hashes.insert((*relative).into(), file.hash.clone()); }
            result.notes.push(format!("PRESERVED {relative}: changed before its ownership guard was acquired; proposal returned."));
            continue;
        }
        event(Event::Guarded, relative, &canonical_root)?;
        if current.as_ref() == Some(&hash) {
            if receipt.files.get(*relative) != Some(&hash) {
                receipt.files.insert((*relative).into(), hash.clone());
                persist_receipt(registry, project, &key, &receipt)?;
            }
            held.insert((*relative).into(), existing.take().expect("guarded identical file"));
            result.outcomes.insert((*relative).into(), "unchanged_receipt_owned");
            continue;
        }
        let mut staged = parent.stage(text)?;
        let recovery_name = existing.as_ref().map(|_| format!(".engram-{}.preimage", uuid::Uuid::new_v4().simple()));
        let recovery = recovery_name.as_ref().map(|name| parent.path.join(name));
        receipt.pending = Some(Pending {
            binding: Some(recovery::binding(&authority, &parent, &proposal_bundle_hash, receipt.files.get(*relative).map(String::as_str))?),
            operation: "publish".into(), relative: (*relative).into(), expected_hash: current.clone(),
            expected_identity: existing.as_ref().map(|file| file.identity.clone()), proposed_hash: Some(hash.clone()),
            staged_identity: Some(staged.identity.clone()),
            staged: Some(staged.path.strip_prefix(&canonical_root)?.to_string_lossy().into_owned()),
            recovery: recovery.as_ref().map(|path| path.strip_prefix(&canonical_root).map(|path| path.to_string_lossy().into_owned())).transpose()?,
            phase: "prepared".into(),
        });
        persist_receipt(registry, project, &key, &receipt)?;
        result.outcomes.insert((*relative).into(), "preserved_unverifiable");
        // From this point the durable journal owns both stage and preimage.
        // Unwinding never discards the recorded recovery evidence.
        staged.retain();
        event(Event::Prepared, relative, &canonical_root)?;
        if let Some(existing) = existing.as_mut() {
            existing.rename(std::ffi::OsStr::new(recovery_name.as_deref().expect("preimage name")))?;
            receipt.pending.as_mut().expect("journal").phase = "retired".into();
            persist_receipt(registry, project, &key, &receipt)?;
            event(Event::Retired, relative, &canonical_root)?;
        }
        let destination = Path::new(relative).file_name().ok_or_else(|| anyhow::anyhow!("Artifact has no filename"))?;
        if let Err(error) = staged.rename(destination) {
            result.outcomes.insert((*relative).into(), "preserved_changed");
            result.notes.push(format!("PRESERVED {relative}: installation did not replace a competing destination ({error}). Pending receipt retains stage {:?} and preimage {:?}; inspect those exact owned recovery paths before clearing the operation.",
                receipt.pending.as_ref().and_then(|pending| pending.staged.as_ref()),
                receipt.pending.as_ref().and_then(|pending| pending.recovery.as_ref())));
            if existing.is_none() {
                // Nothing was retired. Teardown only our held stage; the human
                // winner and the old receipt are preserved without a stranded
                // transaction that would block unrelated future proposals.
                staged.delete()?;
                receipt.pending = None;
                persist_receipt(registry, project, &key, &receipt)?;
            }
            return Err(error);
        }
        event(Event::Installed, relative, &canonical_root)?;
        receipt.files.insert((*relative).into(), hash.clone());
        result.written.push((*relative).into());
        result.observed_hashes.insert((*relative).into(), hash);
        result
            .outcomes
            .insert((*relative).into(), "written_unreceipted");
        receipt.pending.as_mut().expect("journal").phase = "installed".into();
        persist_receipt(registry, project, &key, &receipt)?;
        result.outcomes.insert((*relative).into(), "written");
        held.insert((*relative).into(), staged);
        if let Some(existing) = existing { existing.delete()?; }
        receipt.pending = None;
        persist_receipt(registry, project, &key, &receipt)?;
    }
    let stale = receipt
        .files
        .keys()
        .filter(|path| !desired.contains_key(path.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    for relative in stale {
        // Root policy is not a disposable companion. A default refresh may
        // divert to *.engram.md; that never authorizes removing its original.
        if matches!(relative.as_str(), "CLAUDE.md" | "AGENTS.md") {
            result.notes.push(format!("PRESERVED {relative}: canonical root policy requires an explicit disposition, even when receipt-owned."));
            continue;
        }
        let parent = match guarded::Parent::open(&authority, &relative, false) {
            Ok(parent) => parent,
            Err(error) => {
                result.notes.push(format!("PRESERVED {relative}: stale artifact parent authority unavailable ({error}); review it manually."));
                continue;
            }
        };
        match current_hash(&canonical_root, &relative) {
            Ok(None) => { receipt.files.remove(&relative); },
            Ok(Some(hash)) if receipt.files.get(&relative) == Some(&hash) => {
                event(Event::BeforeRemoval, &relative, &canonical_root)?;
                let existing = match parent.existing(&relative) {
                    Ok(Some(file)) if file.hash == hash => file,
                    _ => {
                        result.notes.push(format!("PRESERVED {relative}: stale artifact changed or a mandatory ownership guard is unavailable; review the returned proposal."));
                        continue;
                    }
                };
                receipt.pending = Some(Pending {
                    binding: Some(recovery::binding(&authority, &parent, &proposal_bundle_hash, receipt.files.get(&relative).map(String::as_str))?),
                    operation: "remove".into(), relative: relative.clone(), expected_hash: Some(hash),
                    expected_identity: Some(existing.identity.clone()), proposed_hash: None, staged_identity: None,
                    staged: None, recovery: None, phase: "prepared".into(),
                });
                persist_receipt(registry, project, &key, &receipt)?;
                existing.delete()?;
                event(Event::Removed, &relative, &canonical_root)?;
                receipt.files.remove(&relative);
                receipt.pending = None;
                persist_receipt(registry, project, &key, &receipt)?;
                result.notes.push(format!("Removed unchanged receipt-owned artifact {relative} (no longer emitted)."));
            },
            _ => result.notes.push(format!("PRESERVED {relative}: stale artifact is edited or ownership cannot be verified; review it manually.")),
        }
    }
    persist_receipt(registry, project, &key, &receipt)?;
    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn companion_chains_are_installed_before_dependent_root_regardless_of_path_order() {
        let temp = tempfile::tempdir().unwrap();
        let registry = Registry::open(&temp.path().join("registry.redb")).unwrap();
        let root = temp.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        let artifacts = vec![
            (
                "AGENTS.md".into(),
                "Read docs/catalog.md before editing.".into(),
            ),
            (
                "docs/catalog.md".into(),
                "Read docs/z-policy.md for the complete accepted policy.".into(),
            ),
            (
                "docs/z-policy.md".into(),
                "Complete policy and exception remain here.".into(),
            ),
        ];
        let dependencies = BTreeMap::from([
            ("AGENTS.md".into(), vec!["docs/catalog.md".into()]),
            ("docs/catalog.md".into(), vec!["docs/z-policy.md".into()]),
        ]);
        let published = publish_bundle(
            &registry,
            "project",
            &root,
            "test",
            &artifacts,
            &dependencies,
            &BTreeMap::new(),
        )
        .unwrap();
        for (path, body) in &artifacts {
            assert_eq!(published.outcomes[path], "written");
            assert_eq!(std::fs::read_to_string(root.join(path)).unwrap(), *body);
        }
        let repeated = publish_bundle(
            &registry,
            "project",
            &root,
            "test",
            &artifacts,
            &dependencies,
            &BTreeMap::new(),
        )
        .unwrap();
        assert!(repeated.written.is_empty());
        for (path, _) in &artifacts {
            assert_eq!(repeated.outcomes[path], "unchanged_receipt_owned");
        }
        drop(registry);
        temp.close().unwrap();
    }

    #[test]
    fn a_cyclic_bundle_is_refused_before_even_independent_artifacts_are_written() {
        let temp = tempfile::tempdir().unwrap();
        let registry = Registry::open(&temp.path().join("registry.redb")).unwrap();
        let root = temp.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        publish(
            &registry,
            "project",
            &root,
            "test",
            &[(
                "previously-owned.md".into(),
                "Keep the prior complete policy.".into(),
            )],
        )
        .unwrap();
        let previous_receipt = registry
            .get_meta("project", "generated_artifacts:test")
            .unwrap();
        let artifacts = vec![
            ("a-independent.md".into(), "No partial publication".into()),
            ("b.md".into(), "Requires c".into()),
            ("c.md".into(), "Requires b".into()),
        ];
        let dependencies = BTreeMap::from([
            ("b.md".into(), vec!["c.md".into()]),
            ("c.md".into(), vec!["b.md".into()]),
        ]);
        let failure = publish_bundle(
            &registry,
            "project",
            &root,
            "test",
            &artifacts,
            &dependencies,
            &BTreeMap::new(),
        )
        .unwrap_err()
        .downcast::<PublicationFailure>()
        .unwrap();
        assert!(failure.report.written.is_empty());
        for (path, _) in &artifacts {
            assert!(!root.join(path).exists());
        }
        assert_eq!(
            std::fs::read_to_string(root.join("previously-owned.md")).unwrap(),
            "Keep the prior complete policy.",
            "A cyclic replacement must not run stale-owned cleanup on the prior bundle"
        );
        assert_eq!(
            registry
                .get_meta("project", "generated_artifacts:test")
                .unwrap(),
            previous_receipt
        );
        drop(registry);
        temp.close().unwrap();
    }

    #[test]
    fn partial_failure_reports_completed_writes_and_does_not_claim_later_artifacts() {
        let temp = tempfile::tempdir().unwrap();
        let registry = Registry::open(&temp.path().join("registry.redb")).unwrap();
        let root = temp.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("blocked"), "A human file, not a directory").unwrap();
        let result = publish(
            &registry,
            "project",
            &root,
            "test",
            &[
                ("a.md".into(), "First complete policy".into()),
                ("blocked/new.md".into(), "Cannot install this".into()),
                ("z.md".into(), "Must not be reported as installed".into()),
            ],
        )
        .unwrap_err()
        .downcast::<PublicationFailure>()
        .unwrap();
        assert_eq!(result.report.written, vec!["a.md"]);
        assert_eq!(result.report.outcomes["a.md"], "written");
        assert_eq!(result.report.outcomes["z.md"], "not_attempted");
        assert_eq!(
            std::fs::read_to_string(root.join("blocked")).unwrap(),
            "A human file, not a directory"
        );
        assert!(!root.join("z.md").exists());
    }

    #[test]
    fn dependency_conflict_preserves_current_root_even_with_explicit_replacement_authorization() {
        let temp = tempfile::tempdir().unwrap();
        let registry = Registry::open(&temp.path().join("registry.redb")).unwrap();
        let root = temp.path().join("repo");
        std::fs::create_dir_all(root.join(".claude/rules")).unwrap();
        std::fs::write(
            root.join("CLAUDE.md"),
            "Complete current policy and exception",
        )
        .unwrap();
        std::fs::write(root.join(".claude/rules/full.md"), "Human companion policy").unwrap();
        let dependency = ".claude/rules/full.md".to_owned();
        let result = publish_bundle(
            &registry,
            "project",
            &root,
            "test",
            &[
                (dependency.clone(), "Proposed complete replacement".into()),
                ("CLAUDE.md".into(), "A short pointer to full.md".into()),
            ],
            &BTreeMap::from([("CLAUDE.md".into(), vec![dependency])]),
            &BTreeMap::from([(
                "CLAUDE.md".into(),
                blake3::hash(b"Complete current policy and exception")
                    .to_hex()
                    .to_string(),
            )]),
        )
        .unwrap();
        assert_eq!(
            result.outcomes["CLAUDE.md"],
            "preserved_dependency_conflict"
        );
        assert!(result.written.is_empty());
        assert_eq!(
            std::fs::read_to_string(root.join("CLAUDE.md")).unwrap(),
            "Complete current policy and exception"
        );
    }

    #[test]
    fn human_guidance_and_edits_survive_refresh_and_stale_cleanup() {
        let temp = tempfile::tempdir().unwrap();
        let registry = Registry::open(&temp.path().join("registry.redb")).unwrap();
        let root = temp.path().join("repo");
        std::fs::create_dir_all(root.join(".claude/rules")).unwrap();
        let human = ".claude/rules/human-conventions.md";
        std::fs::write(root.join(human), "Human policy\r\n").unwrap();
        let files = vec![
            (human.into(), "replacement".into()),
            (".claude/rules/owned.md".into(), "first".into()),
            (".claude/rules/stale.md".into(), "unchanged".into()),
        ];
        let first = publish(&registry, "project", &root, "test", &files).unwrap();
        assert!(
            first
                .notes
                .iter()
                .any(|note| note.contains(human) && note.contains("PRESERVED"))
        );
        assert_eq!(
            std::fs::read(root.join(human)).unwrap(),
            b"Human policy\r\n"
        );
        std::fs::write(root.join(".claude/rules/owned.md"), "Human amendment").unwrap();
        let next = publish(
            &registry,
            "project",
            &root,
            "test",
            &[(".claude/rules/owned.md".into(), "second".into())],
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join(".claude/rules/owned.md")).unwrap(),
            "Human amendment"
        );
        assert!(
            root.join(human).exists(),
            "a familiar stale filename grants no deletion ownership"
        );
        assert!(
            !root.join(".claude/rules/stale.md").exists(),
            "unchanged owned output is retired"
        );
        assert!(
            next.notes
                .iter()
                .any(|note| note.contains("owned.md") && note.contains("PRESERVED"))
        );
    }

    #[test]
    fn moved_project_or_unreadable_receipt_preserves_existing_policy_and_returns_conflicts() {
        let temp = tempfile::tempdir().unwrap();
        let registry = Registry::open(&temp.path().join("registry.redb")).unwrap();
        let original = temp.path().join("original");
        let moved = temp.path().join("moved");
        std::fs::create_dir_all(&original).unwrap();
        std::fs::create_dir_all(&moved).unwrap();
        publish(
            &registry,
            "project",
            &original,
            "test",
            &[("policy.md".into(), "first".into())],
        )
        .unwrap();
        std::fs::write(moved.join("policy.md"), "Human policy at new root").unwrap();
        let moved_result = publish(
            &registry,
            "project",
            &moved,
            "test",
            &[("policy.md".into(), "replacement".into())],
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(moved.join("policy.md")).unwrap(),
            "Human policy at new root"
        );
        assert!(moved_result.written.is_empty());
        assert!(
            moved_result
                .notes
                .iter()
                .any(|note| note.contains("another root/version"))
        );
        registry
            .set_meta("project", "generated_artifacts:test", "unreadable receipt")
            .unwrap();
        let damaged = publish(
            &registry,
            "project",
            &moved,
            "test",
            &[("policy.md".into(), "replacement".into())],
        )
        .unwrap();
        assert!(damaged.written.is_empty());
        assert!(
            damaged
                .notes
                .iter()
                .any(|note| note.contains("receipt is unreadable"))
        );
        assert_eq!(
            std::fs::read_to_string(moved.join("policy.md")).unwrap(),
            "Human policy at new root"
        );
    }

    #[test]
    fn unchanged_owned_output_refreshes_but_identical_unreceipted_text_is_not_adopted() {
        let temp = tempfile::tempdir().unwrap();
        let registry = Registry::open(&temp.path().join("registry.redb")).unwrap();
        let root = temp.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("human.md"), "same").unwrap();
        publish(
            &registry,
            "project",
            &root,
            "test",
            &[
                ("human.md".into(), "same".into()),
                ("owned.md".into(), "first".into()),
            ],
        )
        .unwrap();
        publish(
            &registry,
            "project",
            &root,
            "test",
            &[
                ("human.md".into(), "new".into()),
                ("owned.md".into(), "second".into()),
            ],
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("human.md")).unwrap(),
            "same"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("owned.md")).unwrap(),
            "second"
        );
    }
}

#[cfg(all(test, not(windows)))]
mod unsupported_tests {
    use super::*;

    #[test]
    fn unsupported_publication_returns_proposals_and_preserves_existing_files() {
        let owned = tempfile::tempdir().unwrap();
        let registry = Registry::open(&owned.path().join("registry.redb")).unwrap();
        let root = owned.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("human.md"), b"human instructions").unwrap();
        let failure = publish(&registry, "project", &root, "test", &[
            ("human.md".into(), "human replacement proposal".into()),
            ("new.md".into(), "complete new proposal".into()),
        ]).unwrap_err().downcast::<PublicationFailure>().unwrap();
        assert!(failure.report.written.is_empty());
        assert_eq!(std::fs::read(root.join("human.md")).unwrap(), b"human instructions");
        assert!(!root.join("new.md").exists());
        assert_eq!(failure.report.outcomes["new.md"], "preserved_unverifiable");
        assert!(failure.report.notes.iter().any(|note| note.contains("unavailable on this platform")));
        drop(registry);
        owned.close().unwrap();
    }
}
