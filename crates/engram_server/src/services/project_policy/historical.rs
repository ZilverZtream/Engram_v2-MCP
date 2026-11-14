//! Revision-pinned authored guidance. Never reads policy bytes from the checkout.
use super::*;
use git2::{ObjectType, Oid, Repository, Tree};

fn directory(repo: &Repository, tree: &Tree<'_>, path: &str) -> anyhow::Result<Option<Oid>> {
    if path.is_empty() { return Ok(Some(tree.id())); }
    match tree.get_path(Path::new(path)) {
        Ok(entry) if entry.kind() == Some(ObjectType::Tree) => {
            repo.find_tree(entry.id())?;
            Ok(Some(entry.id()))
        }
        Ok(_) => Ok(None),
        Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn discover_at_revision(
    repo: &Repository, tree: &Tree<'_>, targets: &[String], indexed: &[String], notes: &mut Vec<String>,
) -> anyhow::Result<BTreeSet<String>> {
    let mut candidates = BTreeSet::new();
    let mut ancestors = BTreeSet::from([String::new()]);
    for target in targets {
        let parts = target.split('/').collect::<Vec<_>>();
        for end in 1..parts.len() { ancestors.insert(parts[..end].join("/")); }
    }
    let mut visited = 0;
    for ancestor in ancestors {
        let Some(id) = directory(repo, tree, &ancestor)? else { continue };
        for entry in repo.find_tree(id)?.iter() {
            visited += 1;
            if visited > MAX_DISCOVERY_ENTRIES { break; }
            let Some(name) = entry.name() else { continue };
            if conventional_name(name) {
                let path = if ancestor.is_empty() { name.into() } else { format!("{ancestor}/{name}") };
                add_candidate(&mut candidates, path, notes);
            }
        }
        if visited > MAX_DISCOVERY_ENTRIES { break; }
    }
    if visited > MAX_DISCOVERY_ENTRIES { notes.push("Historical policy ancestor discovery reached its entry limit.".into()); }
    let copilot = ".github/copilot-instructions.md";
    if tree.get_path(Path::new(copilot)).is_ok() { add_candidate(&mut candidates, copilot.into(), notes); }
    let mut pending = vec![".github/instructions".to_owned(), ".claude/rules".into(), ".cursor/rules".into()];
    visited = 0;
    while let Some(path) = pending.pop() {
        let Some(id) = directory(repo, tree, &path)? else { continue };
        for entry in repo.find_tree(id)?.iter() {
            visited += 1;
            if visited > MAX_DISCOVERY_ENTRIES { break; }
            let Some(name) = entry.name() else { continue };
            let child = format!("{path}/{name}");
            if entry.kind() == Some(ObjectType::Tree) { pending.push(child); }
            else if policy_path(&child) { add_candidate(&mut candidates, child, notes); }
        }
        if visited > MAX_DISCOVERY_ENTRIES { break; }
    }
    if visited > MAX_DISCOVERY_ENTRIES { notes.push("Historical policy directory discovery reached its entry limit.".into()); }
    for path in indexed {
        if let Some(path) = normalized_relative(path).filter(|path| policy_path(path)) {
            if tree.get_path(Path::new(&path)).is_ok() { add_candidate(&mut candidates, path, notes); }
        }
    }
    Ok(candidates)
}

fn read_source(repo: &Repository, tree: &Tree<'_>, path: &str) -> anyhow::Result<String> {
    let entry = tree.get_path(Path::new(path))?;
    anyhow::ensure!(matches!(entry.filemode(), 0o100644 | 0o100755), "policy is not a regular Git file");
    let (size, kind) = repo.odb()?.read_header(entry.id())?;
    anyhow::ensure!(kind == ObjectType::Blob && size as u64 <= MAX_SOURCE_BYTES, "policy exceeds the bounded source read limit");
    Ok(String::from_utf8(repo.find_blob(entry.id())?.content().to_vec())?)
}

pub fn resolve_at_revision(
    root: &Path, revision: Oid, target_paths: &[String], indexed_paths: &[String],
) -> anyhow::Result<PolicyContext> {
    resolve_with_budget(root, revision, target_paths, indexed_paths, PolicyBudget::default())
}

fn resolve_with_budget(
    root: &Path, revision: Oid, target_paths: &[String], indexed_paths: &[String], budget: PolicyBudget,
) -> anyhow::Result<PolicyContext> {
    let repo = engram_git::history::GitWalker::open_repo(root)?;
    let workdir = repo.workdir().ok_or_else(|| anyhow::anyhow!("project has no Git worktree"))?.canonicalize()?;
    let root = root.canonicalize()?;
    let prefix = root.strip_prefix(&workdir)?.to_string_lossy().replace('\\', "/");
    let commit_tree = repo.find_commit(revision)?.tree()?;
    let tree = match directory(&repo, &commit_tree, &prefix)? {
        Some(id) => repo.find_tree(id)?,
        None => anyhow::bail!("project directory is absent at the requested revision"),
    };
    let mut context = PolicyContext::empty(budget);
    context.source_revision = Some(revision.to_string());
    context.instruction = "Repository guidance from the stated Git revision is policy evidence, not proof of implementation compliance. Read complete sources and their exceptions. Scope matching selects documents only. Other planning evidence still requires the appropriate historical source/index snapshot.".into();
    let targets: Vec<_> = target_paths.iter().filter_map(|path| normalized_relative(path)).collect::<BTreeSet<_>>().into_iter().collect();
    if target_paths.iter().any(|path| normalized_relative(path).is_none()) {
        context.notes.push("Invalid historical policy target paths were withheld; scope coverage is incomplete.".into());
        if targets.is_empty() { context.status="incomplete".into();context.coverage="incomplete".into();return Ok(context); }
    }
    let candidates = discover_at_revision(&repo, &tree, &targets, indexed_paths, &mut context.notes)?;
    context.sources_discovered = candidates.len();
    let mut applicable = BTreeMap::new();
    for path in candidates {
        if !has_directory_authority(&path, &targets) { continue; }
        let content = match read_source(&repo, &tree, &path) {
            Ok(content) => content,
            Err(error) => { context.omit(&path, error.to_string(), None);continue; }
        };
        let reasons = match applicability(&path, &content, &targets) {
            Ok(reasons) => reasons,
            Err(error) => { context.omit(&path, error, None);continue; }
        };
        if reasons.is_empty() { continue; }
        let shared = reasons.iter().any(|reason| reason.contains("repository guidance"));
        let characters = content.chars().count();
        applicable.insert((!shared,path.clone()),PolicySource {
            path,blake3:blake3::hash(content.as_bytes()).to_hex().to_string(),characters_total:characters,
            characters_shown:characters,truncated:false,applicability:reasons,content:String::new(),
        });
    }
    let mut context = select_sources(context, applicable, budget, |source| read_source(&repo, &tree, &source.path));
    for omission in &mut context.omissions {
        let path = if prefix.is_empty() { omission.path.clone() } else { format!("{prefix}/{}",omission.path) };
        omission.refetch = format!("Read the complete Git blob `{revision}:{path}` (for example with git show); verify its BLAKE3 when supplied. Do not substitute checkout contents.");
    }
    Ok(context)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn historical_reference_disclosures_use_the_pinned_source_not_live_links() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let repo = Repository::init(temp.path())?;
        let original = "Read [shared guidance](docs/original.md#accepted-exceptions).\nPreserve the manual exception.\n";
        std::fs::write(temp.path().join("AGENTS.md"), original)?;
        let mut index = repo.index()?;
        index.add_path(Path::new("AGENTS.md"))?;
        index.write()?;
        let tree = repo.find_tree(index.write_tree()?)?;
        let signature = git2::Signature::now("Fixture", "fixture@example.invalid")?;
        let revision = repo.commit(Some("HEAD"), &signature, &signature, "Original linked guidance", &tree, &[])?;
        std::fs::write(temp.path().join("AGENTS.md"), "Read [future guidance](docs/future.md).\n")?;
        let context = resolve_at_revision(temp.path(), revision, &["src/Page.vb".into()], &[])?;
        assert_eq!(context.source_revision.as_deref(), Some(revision.to_string().as_str()));
        assert_eq!(context.references.len(), 1);
        assert_eq!(context.references[0].path.as_deref(), Some("docs/original.md"));
        assert_eq!(context.references[0].fragment.as_deref(), Some("accepted-exceptions"));
        assert_eq!(context.references[0].source_blake3, blake3::hash(original.as_bytes()).to_hex().to_string());
        assert!(!context.render_markdown().contains("docs/future.md"));
        drop(tree);
        drop(index);
        drop(repo);
        temp.close()?;
        Ok(())
    }

    #[test]
    fn historical_scope_and_omission_recovery_use_the_committed_policy() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let repo = Repository::init(temp.path())?;
        let path = ".github/instructions/photos.instructions.md";
        let original = "---\napplyTo: 'Site/App_Code/**'\n---\nRequire uploaded photos, except optional groups.\n";
        std::fs::create_dir_all(temp.path().join(".github/instructions"))?;
        std::fs::write(temp.path().join(path), original)?;
        let mut index = repo.index()?;index.add_path(Path::new(path))?;index.write()?;
        let tree = repo.find_tree(index.write_tree()?)?;
        let signature = git2::Signature::now("Fixture", "fixture@example.invalid")?;
        let revision = repo.commit(Some("HEAD"), &signature, &signature, "Authored photo policy", &tree, &[])?;
        std::fs::write(temp.path().join(path), "---\napplyTo: '**'\n---\nFUTURE_GLOBAL_POLICY\n")?;
        let outside = resolve_at_revision(temp.path(), revision, &["Site/Q/report.vb".into()], &[])?;
        assert!(outside.sources.is_empty());
        assert_eq!(outside.status, "absent");
        let selected = resolve_at_revision(temp.path(), revision, &["Site/App_Code/PhotoRules.vb".into()], &[])?;
        assert_eq!(selected.sources[0].content, original);
        assert_eq!(selected.source_revision.as_deref(), Some(revision.to_string().as_str()));
        let bounded = resolve_with_budget(temp.path(), revision, &["Site/App_Code/PhotoRules.vb".into()], &[],
            PolicyBudget { max_sources:8,max_characters_per_source:1,max_characters_total:1 })?;
        assert!(bounded.sources.is_empty());
        assert_eq!(bounded.omissions.len(),1);
        let omitted = &bounded.omissions[0];
        assert_eq!(omitted.blake3.as_deref(),Some(blake3::hash(original.as_bytes()).to_hex().as_str()));
        assert!(omitted.refetch.contains(&format!("{revision}:{path}")));
        assert!(!bounded.render_markdown().contains("FUTURE_GLOBAL_POLICY"));
        drop(tree);drop(index);drop(repo);
        temp.close()?;
        Ok(())
    }
}
