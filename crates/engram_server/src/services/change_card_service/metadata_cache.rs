//! Derived metadata only. Immutable Git OIDs retain the existing warm-memory
//! trust semantics across restart; this is not evidence that every historical
//! tree/blob remains recoverable. Git traversal/root-tree failures still fail.
use super::*;
use engram_core::Registry;
use std::collections::{HashMap, HashSet};

pub(super) trait Store {
    fn lookup(&self, keys: &[String]) -> anyhow::Result<Vec<Option<Vec<u8>>>>;
    fn persist(
        &self,
        entries: &[(String, Vec<u8>)],
    ) -> anyhow::Result<engram_core::registry::ChangeCardCacheWrite>;
}
impl Store for Registry {
    fn lookup(&self, keys: &[String]) -> anyhow::Result<Vec<Option<Vec<u8>>>> {
        self.read_change_card_metadata(keys)
    }
    fn persist(
        &self,
        entries: &[(String, Vec<u8>)],
    ) -> anyhow::Result<engram_core::registry::ChangeCardCacheWrite> {
        self.store_change_card_metadata(entries)
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    oid: String,
    title: String,
    description: String,
    author: String,
    timestamp: u64,
    files: Vec<String>,
    file_count: usize,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u32,
    scope: String,
    metadata: Metadata,
    checksum: String,
}

enum Decoded {
    Hit(ChangeCard),
    Miss,
    Future,
}

fn scope(root: &Path, repo: &Repository) -> anyhow::Result<String> {
    // Include extraction implementation, rather than relying on developers to
    // remember a version bump after changing generated filters/title parsing.
    static EXTRACTOR: LazyLock<blake3::Hash> = LazyLock::new(|| {
        let mut hash = blake3::Hasher::new();
        for source in [
            include_str!("../change_card_service.rs"),
            include_str!("metadata_cache.rs"),
            include_str!("../../handlers/pr_history_tools.rs"),
        ] {
            hash.update(&(source.len() as u64).to_le_bytes());
            hash.update(source.as_bytes());
        }
        hash.finalize()
    });
    let mut hash = blake3::Hasher::new();
    hash.update(b"engram-change-card-metadata-v1");
    hash.update(EXTRACTOR.as_bytes());
    for path in [root.canonicalize()?, repo.path().canonicalize()?] {
        let bytes = path.as_os_str().as_encoded_bytes();
        hash.update(&(bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    }
    Ok(hash.finalize().to_hex().to_string())
}

fn encode(scope: &str, card: &ChangeCard) -> anyhow::Result<Vec<u8>> {
    let metadata = Metadata {
        oid: card.oid.clone(),
        title: card.title.clone(),
        description: card.description.clone(),
        author: card.author.clone(),
        timestamp: card.timestamp,
        files: card.files.clone(),
        file_count: card.file_count,
    };
    let checksum = blake3::hash(&serde_json::to_vec(&metadata)?)
        .to_hex()
        .to_string();
    Ok(serde_json::to_vec(&Envelope {
        version: 1,
        scope: scope.into(),
        metadata,
        checksum,
    })?)
}

fn decode(bytes: &[u8], scope: &str, oid: Oid) -> Decoded {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return Decoded::Miss;
    };
    if value
        .get("version")
        .and_then(|v| v.as_u64())
        .is_some_and(|v| v != 1)
    {
        return Decoded::Future;
    }
    let Some(object) = value.as_object() else {
        return Decoded::Miss;
    };
    if object
        .keys()
        .any(|key| !["version", "scope", "metadata", "checksum"].contains(&key.as_str()))
    {
        return Decoded::Future;
    }
    if object
        .get("metadata")
        .and_then(|v| v.as_object())
        .is_some_and(|row| {
            row.keys().any(|key| {
                ![
                    "oid",
                    "title",
                    "description",
                    "author",
                    "timestamp",
                    "files",
                    "file_count",
                ]
                .contains(&key.as_str())
            })
        })
    {
        return Decoded::Future;
    }
    let Ok(envelope) = serde_json::from_value::<Envelope>(value) else {
        return Decoded::Miss;
    };
    if envelope.version != 1 || envelope.scope != scope || envelope.metadata.oid != oid.to_string()
    {
        return Decoded::Miss;
    }
    let row = envelope.metadata;
    if row.files.len() > MAX_CARD_FILES
        || row.file_count < row.files.len()
        || (row.file_count <= MAX_CARD_FILES && row.files.len() != row.file_count)
        || row.description.chars().count() > MAX_DESCRIPTION
    {
        return Decoded::Miss;
    }
    let Ok(encoded) = serde_json::to_vec(&row) else {
        return Decoded::Miss;
    };
    if blake3::hash(&encoded).to_hex().as_str() != envelope.checksum {
        return Decoded::Miss;
    }
    Decoded::Hit(ChangeCard {
        oid: row.oid,
        title: row.title,
        description: row.description,
        author: row.author,
        timestamp: row.timestamp,
        files: row.files,
        file_count: row.file_count,
        vector: Vec::new(),
    })
}

pub(super) fn read(
    root: &Path,
    tip: Oid,
    known: &HashSet<String>,
    registry: Option<&dyn Store>,
) -> anyhow::Result<(Vec<Oid>, Vec<ChangeCard>, CardBuildStats)> {
    let opened = std::time::Instant::now();
    let repo = engram_git::history::GitWalker::open_repo(root)?;
    let mut stats = CardBuildStats {
        git_open_ns: opened.elapsed().as_nanos() as u64,
        ..Default::default()
    };
    let walked = std::time::Instant::now();
    let line = first_parent_line(&repo, tip)?;
    stats.first_parent_walk_ns = walked.elapsed().as_nanos() as u64;
    stats.line_commits = line.len();
    let missing: Vec<_> = line
        .iter()
        .copied()
        .filter(|oid| !known.contains(&oid.to_string()))
        .collect();
    let scope = registry.map(|_| scope(root, &repo)).transpose()?;
    let mut cached = HashMap::new();
    let mut cached_lengths = HashMap::new();
    let mut future = HashSet::new();
    if let (Some(registry), Some(scope)) = (registry, scope.as_ref()) {
        for chunk in missing.chunks(engram_core::registry::CHANGE_CARD_CACHE_BATCH) {
            let keys: Vec<_> = chunk.iter().map(|oid| format!("{scope}/{oid}")).collect();
            stats.metadata_cache_lookups += keys.len();
            match registry.lookup(&keys) {
                Ok(rows) => {
                    for ((&oid, key), row) in chunk.iter().zip(keys).zip(rows) {
                        if let Some(row) = row {
                            match decode(&row, scope, oid) {
                                Decoded::Hit(card) => {
                                    cached.insert(oid, card);
                                    cached_lengths.insert(oid, row.len());
                                    stats.metadata_cache_hits += 1;
                                }
                                Decoded::Future => {
                                    future.insert(key);
                                }
                                Decoded::Miss => {}
                            }
                        }
                    }
                }
                Err(_) => {
                    stats.metadata_cache_errors += 1;
                }
            }
        }
    }
    let retain: HashSet<_> = line
        .iter()
        .rev()
        .take(engram_core::registry::CHANGE_CARD_CACHE_ROWS)
        .copied()
        .collect();
    let mut fresh = Vec::with_capacity(missing.len());
    for oid in missing {
        if let Some(card) = cached.remove(&oid) {
            // This matches the immutable-OID cache model, with additional cheap
            // root-tree availability checks. Recursive objects remain unproven.
            let commit = repo.find_commit(oid)?;
            commit.tree()?;
            if commit.parent_count() != 0 {
                commit.parent(0)?.tree()?;
            }
            fresh.push(card);
            continue;
        }
        let described = std::time::Instant::now();
        let card = describe(&repo, oid)?;
        stats.describe_ns += described.elapsed().as_nanos() as u64;
        stats.described_commits += 1;
        fresh.push(card);
    }
    if let (Some(registry), Some(scope)) = (registry, scope.as_ref()) {
        let mut pending = Vec::new();
        let mut pending_bytes = 0;
        let mut reserved = 0usize;
        let mut admitted = Vec::new();
        for card in fresh.iter().rev() {
            let oid = Oid::from_str(&card.oid)?;
            if !retain.contains(&oid) {
                continue;
            }
            let key = format!("{scope}/{oid}");
            if let Some(&length) = cached_lengths.get(&oid) {
                reserved = reserved.saturating_add(2 * key.len() + length + 24);
                continue;
            }
            let raw_bytes = [&card.oid, &card.title, &card.description, &card.author]
                .iter()
                .map(|s| s.len())
                .chain(card.files.iter().map(String::len))
                .fold(0usize, usize::saturating_add);
            if future.contains(&key)
                || raw_bytes > engram_core::registry::CHANGE_CARD_CACHE_RECORD_BYTES
            {
                continue;
            }
            let encoded = encode(scope, card)?;
            if encoded.len() > engram_core::registry::CHANGE_CARD_CACHE_RECORD_BYTES {
                continue;
            }
            let charge = 2 * key.len() + encoded.len() + 24;
            if reserved.saturating_add(charge) > engram_core::registry::CHANGE_CARD_CACHE_BYTES {
                continue;
            }
            reserved += charge;
            admitted.push(card);
        }
        let mut flush = |pending: &mut Vec<(String, Vec<u8>)>| {
            if pending.is_empty() {
                return;
            }
            match registry.persist(pending) {
                Ok(result) => {
                    stats.metadata_cache_writes += result.stored;
                    stats.metadata_cache_evictions += result.evicted;
                    stats.metadata_cache_skipped += result.skipped;
                }
                Err(_) => {
                    stats.metadata_cache_errors += 1;
                }
            }
            pending.clear();
        };
        // Keep the newest bounded working set stable, even when the requested
        // line is larger than the cache. Do not rewrite all older misses on
        // each query, or hold another full serialized history in memory.
        // Insert oldest first within the admitted newest set, so FIFO expiry
        // does not discard its newest commits before its older commits.
        for card in admitted.into_iter().rev() {
            let oid = Oid::from_str(&card.oid)?;
            let key = format!("{scope}/{oid}");
            let encoded = encode(scope, card)?;
            pending_bytes += encoded.len();
            pending.push((key, encoded));
            if pending.len() == engram_core::registry::CHANGE_CARD_CACHE_BATCH
                || pending_bytes >= 1024 * 1024
            {
                flush(&mut pending);
                pending_bytes = 0;
            }
        }
        flush(&mut pending);
    }
    if stats.metadata_cache_errors != 0 {
        tracing::warn!(
            failures = stats.metadata_cache_errors,
            "Change-card metadata cache unavailable; Git results remain authoritative"
        );
    }
    Ok((line, fresh, stats))
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture {
        registry: Registry,
        repo: Repository,
        owned: tempfile::TempDir,
    }
    impl Fixture {
        fn new() -> anyhow::Result<Self> {
            let owned = tempfile::tempdir()?;
            let registry = Registry::open(&owned.path().join("registry.redb"))?;
            let repo = Repository::init(owned.path().join("repo"))?;
            Ok(Self {
                registry,
                repo,
                owned,
            })
        }
        fn root(&self) -> &Path {
            self.repo.workdir().unwrap()
        }
    }
    fn commit(repo: &Repository, number: i64) -> anyhow::Result<Oid> {
        let parent = repo.head().ok().and_then(|head| head.peel_to_commit().ok());
        let blob = repo.blob(format!("pub fn revision_{number}() {{}}\n").as_bytes())?;
        let tree = {
            let mut builder = repo.treebuilder(None)?;
            builder.insert("file.rs", blob, 0o100644)?;
            repo.find_tree(builder.write()?)?
        };
        let signature = git2::Signature::new(
            "Fixture",
            "fixture@example.invalid",
            &git2::Time::new(1_700_000_000 + number, 0),
        )?;
        Ok(repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            &format!("Feature {number}\n\nPreserve the qualification."),
            &tree,
            &parent.iter().collect::<Vec<_>>(),
        )?)
    }
    fn verify(card: &ChangeCard, number: i64) {
        assert_eq!(card.title, format!("Feature {number}"));
        assert_eq!(card.description, "Preserve the qualification.");
        assert_eq!(card.author, "Fixture");
        assert_eq!(card.timestamp, (1_700_000_000 + number) as u64);
        assert_eq!(card.files, vec!["file.rs"]);
        assert_eq!(card.file_count, 1);
    }
    #[test]
    fn advancing_and_selecting_an_older_line_reuses_only_its_metadata() -> anyhow::Result<()> {
        let f = Fixture::new()?;
        let first = commit(&f.repo, 0)?;
        let (_, rows, cold) = read(f.root(), first, &Default::default(), Some(&f.registry))?;
        assert_eq!(cold.described_commits, 1);
        verify(&rows[0], 0);
        let second = commit(&f.repo, 1)?;
        let (_, rows, moved) = read(f.root(), second, &Default::default(), Some(&f.registry))?;
        assert_eq!(moved.described_commits, 1);
        assert_eq!(moved.metadata_cache_hits, 1);
        verify(&rows[0], 0);
        verify(&rows[1], 1);
        let (_, rows, older) = read(f.root(), first, &Default::default(), Some(&f.registry))?;
        assert_eq!(rows.len(), 1);
        verify(&rows[0], 0);
        assert_eq!(older.described_commits, 0);
        assert_eq!(older.metadata_cache_writes, 0);
        Ok(())
    }
    #[test]
    fn identical_oids_in_independent_repositories_do_not_cross_hit() -> anyhow::Result<()> {
        let f = Fixture::new()?;
        let first = commit(&f.repo, 0)?;
        read(f.root(), first, &Default::default(), Some(&f.registry))?;
        let other = Repository::init(f.owned.path().join("other"))?;
        let second = commit(&other, 0)?;
        assert_eq!(first, second);
        let (_, rows, stats) = read(
            other.workdir().unwrap(),
            second,
            &Default::default(),
            Some(&f.registry),
        )?;
        assert_eq!(stats.metadata_cache_hits, 0);
        assert_eq!(stats.described_commits, 1);
        verify(&rows[0], 0);
        Ok(())
    }
    #[test]
    fn corrupt_records_recompute_and_future_formats_are_preserved() -> anyhow::Result<()> {
        let f = Fixture::new()?;
        let tip = commit(&f.repo, 0)?;
        read(f.root(), tip, &Default::default(), Some(&f.registry))?;
        let key = format!("{}/{tip}", scope(f.root(), &f.repo)?);
        for bad in [
            b"not json".to_vec(),
            br#"{"version":1,"scope":"wrong","checksum":"bad","metadata":{}}"#.to_vec(),
        ] {
            f.registry
                .store_change_card_metadata(&[(key.clone(), bad)])?;
            let (_, rows, stats) = read(f.root(), tip, &Default::default(), Some(&f.registry))?;
            assert_eq!(stats.described_commits, 1);
            verify(&rows[0], 0);
        }
        let future = br#"{"version":2,"future":"preserve this"}"#.to_vec();
        f.registry
            .store_change_card_metadata(&[(key.clone(), future.clone())])?;
        let (_, rows, stats) = read(f.root(), tip, &Default::default(), Some(&f.registry))?;
        verify(&rows[0], 0);
        assert_eq!(stats.metadata_cache_writes, 0);
        assert_eq!(
            f.registry.read_change_card_metadata(&[key])?[0],
            Some(future)
        );
        Ok(())
    }
    #[test]
    fn unavailable_optional_persistence_does_not_discard_valid_git_results() -> anyhow::Result<()> {
        struct Unavailable;
        impl Store for Unavailable {
            fn lookup(&self, _: &[String]) -> anyhow::Result<Vec<Option<Vec<u8>>>> {
                anyhow::bail!("read unavailable")
            }
            fn persist(
                &self,
                _: &[(String, Vec<u8>)],
            ) -> anyhow::Result<engram_core::registry::ChangeCardCacheWrite> {
                anyhow::bail!("write unavailable")
            }
        }
        let f = Fixture::new()?;
        let tip = commit(&f.repo, 0)?;
        let (_, rows, stats) = read(f.root(), tip, &Default::default(), Some(&Unavailable))?;
        verify(&rows[0], 0);
        assert_eq!(stats.metadata_cache_errors, 2);
        Ok(())
    }
    #[test]
    fn cached_metadata_cannot_hide_a_missing_ancestor() -> anyhow::Result<()> {
        let f = Fixture::new()?;
        let first = commit(&f.repo, 0)?;
        let tip = commit(&f.repo, 1)?;
        read(f.root(), tip, &Default::default(), Some(&f.registry))?;
        let hex = first.to_string();
        let object = f
            .repo
            .path()
            .join("objects")
            .join(&hex[..2])
            .join(&hex[2..]);
        assert!(
            object
                .canonicalize()?
                .starts_with(f.owned.path().canonicalize()?)
        );
        std::fs::remove_file(object)?;
        assert!(read(f.root(), tip, &Default::default(), Some(&f.registry)).is_err());
        Ok(())
    }
    #[test]
    fn cached_metadata_cannot_hide_an_unavailable_root_tree() -> anyhow::Result<()> {
        let f = Fixture::new()?;
        let tip = commit(&f.repo, 0)?;
        read(f.root(), tip, &Default::default(), Some(&f.registry))?;
        let hex = f.repo.find_commit(tip)?.tree_id().to_string();
        let object = f
            .repo
            .path()
            .join("objects")
            .join(&hex[..2])
            .join(&hex[2..]);
        assert!(
            object
                .canonicalize()?
                .starts_with(f.owned.path().canonicalize()?)
        );
        std::fs::remove_file(object)?;
        assert!(read(f.root(), tip, &Default::default(), Some(&f.registry)).is_err());
        Ok(())
    }

    #[test]
    fn extraction_does_not_treat_an_unavailable_parent_as_a_root_change() -> anyhow::Result<()> {
        let f = Fixture::new()?;
        let parent = commit(&f.repo, 0)?;
        let tip = commit(&f.repo, 1)?;
        let hex = parent.to_string();
        let object = f
            .repo
            .path()
            .join("objects")
            .join(&hex[..2])
            .join(&hex[2..]);
        assert!(
            object
                .canonicalize()?
                .starts_with(f.owned.path().canonicalize()?)
        );
        std::fs::remove_file(object)?;
        let reopened = Repository::open(f.root())?;
        assert!(
            describe(&reopened, tip).is_err(),
            "A missing recorded parent must not become an initial-commit diff"
        );
        Ok(())
    }
}
