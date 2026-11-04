//! Reconcile only directory-bound, byte-identical interrupted publications.
//! Recovery never replays an old proposal. It restores a verified preimage or
//! finalizes the exact already-installed staged object, then ordinary guarded
//! publication evaluates the current bundle/dependencies again.
use super::*;

#[derive(Clone, Serialize, Deserialize)]
enum PriorReceipt {
    Absent,
    Present(String),
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Binding {
    version: u32,
    root_identity: String,
    parent_identity: String,
    proposal_bundle_hash: String,
    prior_receipt: PriorReceipt,
}

pub(super) fn binding(
    root: &guarded::Root,
    parent: &guarded::Parent,
    bundle: &str,
    prior: Option<&str>,
) -> anyhow::Result<Binding> {
    Ok(Binding {
        version: 2,
        root_identity: root.identity()?,
        parent_identity: parent.identity()?,
        proposal_bundle_hash: bundle.into(),
        prior_receipt: match prior {
            Some(hash) => PriorReceipt::Present(hash.into()),
            None => PriorReceipt::Absent,
        },
    })
}

pub(super) fn bundle_hash(
    desired: &BTreeMap<&str, &str>,
    dependencies: &BTreeMap<String, Vec<String>>,
    preimages: &BTreeMap<String, String>,
    create_only: &std::collections::BTreeSet<&str>,
) -> anyhow::Result<String> {
    anyhow::ensure!(
        dependencies.len() <= 512 && preimages.len() <= 512,
        "Publication recovery input limit exceeded"
    );
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"engram-publication-bundle-v1");
    fn part(hash: &mut blake3::Hasher, text: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            text.len() <= 4096,
            "Publication recovery path/hash limit exceeded"
        );
        hash.update(&(text.len() as u64).to_le_bytes());
        hash.update(text.as_bytes());
        Ok(())
    }
    for (path, text) in desired {
        anyhow::ensure!(
            text.len() as u64 <= MAX_FILE_BYTES,
            "Generated artifact exceeds write limit"
        );
        hasher.update(&[0]);
        part(&mut hasher, path)?;
        part(
            &mut hasher,
            &blake3::hash(text.as_bytes()).to_hex().to_string(),
        )?;
    }
    let mut edges = 0usize;
    for (path, required) in dependencies {
        hasher.update(&[1]);
        part(&mut hasher, path)?;
        hasher.update(&(required.len() as u64).to_le_bytes());
        for dependency in required {
            edges = edges
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("Publication dependency count overflow"))?;
            anyhow::ensure!(
                edges <= 4096,
                "Publication recovery dependency budget exceeded"
            );
            part(&mut hasher, dependency)?;
        }
    }
    for (path, hash) in preimages {
        hasher.update(&[2]);
        part(&mut hasher, path)?;
        part(&mut hasher, hash)?;
    }
    for path in create_only {
        hasher.update(&[3]);
        part(&mut hasher, path)?;
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn owned_path(
    parent: &guarded::Parent,
    root: &Path,
    relative: &str,
    suffix: &str,
) -> anyhow::Result<()> {
    let path = engram_core::safe_join(root, relative)?;
    anyhow::ensure!(
        path.parent() == Some(parent.path.as_path()),
        "Recovery evidence has a different parent; preserve journal"
    );
    let token = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(".engram-"))
        .and_then(|name| name.strip_suffix(suffix))
        .ok_or_else(|| anyhow::anyhow!("Unrecognized recovery filename; preserve journal"))?;
    anyhow::ensure!(
        token.len() == 32 && token.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Unrecognized recovery ownership token; preserve journal"
    );
    Ok(())
}

fn same(file: &guarded::HeldFile, identity: &Option<String>, hash: &Option<String>) -> bool {
    identity.as_ref() == Some(&file.identity) && hash.as_ref() == Some(&file.hash)
}

/// Returns false without mutation when the durable record cannot prove every
/// affected object. Old journals have no directory binding and remain proposals.
/// The caller retains the checked project root and its update lock throughout.
pub(super) fn reconcile(
    registry: &Registry,
    project: &str,
    key: &str,
    root: &guarded::Root,
    receipt: &mut Receipt,
    bundle: &str,
    desired: &BTreeMap<&str, &str>,
    preimages: &BTreeMap<String, String>,
    result: &mut Publication,
) -> anyhow::Result<bool> {
    let Some(pending) = receipt.pending.clone() else {
        return Ok(true);
    };
    let Some(bound) = &pending.binding else {
        return Ok(false);
    };
    if bound.version != 2
        || bound.proposal_bundle_hash != bundle
        || bound.root_identity != root.identity()?
    {
        return Ok(false);
    }
    if !matches!(
        pending.phase.as_str(),
        "prepared" | "retired" | "installed" | "restoring"
    ) {
        return Ok(false);
    }
    let parent = guarded::Parent::open(root, &pending.relative, false)?;
    if bound.parent_identity != parent.identity()? {
        return Ok(false);
    }
    let mut current = parent.existing(&pending.relative)?;
    if pending.operation == "remove" {
        if desired.contains_key(pending.relative.as_str())
            || pending.expected_hash.is_none()
            || pending.expected_identity.is_none()
            || pending.proposed_hash.is_some()
            || pending.staged_identity.is_some()
            || pending.staged.is_some()
            || pending.recovery.is_some()
            || receipt.files.get(&pending.relative) != pending.expected_hash.as_ref()
        {
            return Ok(false);
        }
        if let Some(file) = current.take() {
            if !same(&file, &pending.expected_identity, &pending.expected_hash) {
                return Ok(false);
            }
            file.delete()?;
        }
        receipt.files.remove(&pending.relative);
        receipt.pending = None;
        persist_receipt(registry, project, key, receipt)?;
        result.notes.push(format!(
            "Reconciled interrupted removal of verified owned artifact {}.",
            pending.relative
        ));
        return Ok(true);
    }
    if pending.operation != "publish" {
        return Ok(false);
    }
    let Some(text) = desired.get(pending.relative.as_str()) else {
        return Ok(false);
    };
    if pending.proposed_hash.as_deref() != Some(blake3::hash(text.as_bytes()).to_hex().as_str())
        || pending.staged_identity.is_none()
        || pending.staged.is_none()
        || pending.expected_hash.is_some() != pending.expected_identity.is_some()
        || pending.expected_hash.is_some() != pending.recovery.is_some()
    {
        return Ok(false);
    }
    if let Some(expected) = &pending.expected_hash {
        let source_owned = matches!(&bound.prior_receipt, PriorReceipt::Present(hash) if hash == expected)
            || preimages.get(&pending.relative) == Some(expected);
        let already_receipted = pending.phase == "installed"
            && receipt.files.get(&pending.relative) == pending.proposed_hash.as_ref();
        if !source_owned && !already_receipted {
            return Ok(false);
        }
    }
    let staged_path = pending.staged.as_deref().expect("validated stage path");
    owned_path(&parent, &root.path, staged_path, ".stage")?;
    let staged = parent.existing(staged_path)?;
    if staged
        .as_ref()
        .is_some_and(|file| !same(file, &pending.staged_identity, &pending.proposed_hash))
    {
        return Ok(false);
    }
    let mut preimage = if let Some(path) = pending.recovery.as_deref() {
        owned_path(&parent, &root.path, path, ".preimage")?;
        parent.existing(path)?
    } else {
        None
    };
    if preimage
        .as_ref()
        .is_some_and(|file| !same(file, &pending.expected_identity, &pending.expected_hash))
    {
        return Ok(false);
    }
    let installed = current
        .as_ref()
        .is_some_and(|file| same(file, &pending.staged_identity, &pending.proposed_hash));
    if installed {
        if staged.is_some()
            || (pending.expected_hash.is_some()
                && preimage.is_none()
                && pending.phase != "installed")
        {
            return Ok(false);
        }
        // First make the installed truth durable. The held target excludes
        // edits until preimage cleanup and the final pending clear complete.
        receipt.files.insert(
            pending.relative.clone(),
            pending.proposed_hash.clone().expect("validated proposal"),
        );
        receipt.pending.as_mut().expect("live journal").phase = "installed".into();
        persist_receipt(registry, project, key, receipt)?;
        if let Some(file) = preimage.take() {
            file.delete()?;
        }
        receipt.pending = None;
        persist_receipt(registry, project, key, receipt)?;
        result.notes.push(format!(
            "Reconciled already-installed verified owned artifact {}.",
            pending.relative
        ));
        return Ok(true);
    }
    if let Some(file) = &current {
        if !same(file, &pending.expected_identity, &pending.expected_hash) || preimage.is_some() {
            return Ok(false); // A human winner is never renamed, adopted or deleted.
        }
    } else if pending.expected_hash.is_some() {
        let Some(mut file) = preimage.take() else {
            return Ok(false);
        };
        // Persist intent before restoring. A crash after rename is recognized
        // on retry by the original object's identity/hash at its original path.
        receipt.pending.as_mut().expect("live journal").phase = "restoring".into();
        persist_receipt(registry, project, key, receipt)?;
        let name = Path::new(&pending.relative)
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("Recovery destination has no filename"))?;
        file.rename(name)?; // no replacement, including a human winner in the gap
        current = Some(file);
    }
    if let Some(file) = staged {
        file.delete()?;
    }
    // Restore the exact PRIOR receipt entry. The verified human preimage may
    // have been authorized only for this replacement, with no prior receipt or
    // a stale generated hash; restoring its bytes never grants new ownership.
    match &bound.prior_receipt {
        PriorReceipt::Present(hash) => {
            receipt.files.insert(pending.relative.clone(), hash.clone());
        }
        PriorReceipt::Absent => {
            receipt.files.remove(&pending.relative);
        }
    }
    receipt.pending = None;
    persist_receipt(registry, project, key, receipt)?;
    result.notes.push(format!("Reconciled interrupted publication of {}; verified preimage preserved before current proposal evaluation.", pending.relative));
    drop(current);
    Ok(true)
}
