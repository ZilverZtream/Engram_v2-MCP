//! Scoped cold history reads must not construct/cache unrelated commit cards.
use anyhow::{Result, ensure};
use engram_core::Registry;
use engram_server::services::change_card_service::{CardFilter, metadata_for_line_filtered_cached};
use redb::{ReadableTable, TableDefinition};
use std::{collections::BTreeSet, path::Path};

struct Fixture {
    registry: Option<Registry>,
    root: std::path::PathBuf,
    commits: Vec<git2::Oid>,
    owned: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let owned = tempfile::Builder::new().prefix("hf-").tempdir_in(parent)?;
        let root = owned.path().join("r");
        std::fs::create_dir_all(root.join("Site"))?;
        let repo = git2::Repository::init(&root)?;
        let mut commits = Vec::new();
        for n in 0..6 {
            std::fs::write(
                root.join("Site/preview.ts"),
                format!("export function preview() {{ return {n}; }}\n"),
            )?;
            let mut index = repo.index()?;
            index.add_path(Path::new("Site/preview.ts"))?;
            let tree = repo.find_tree(index.write_tree()?)?;
            let name = if n % 2 == 0 { "Ada" } else { "Ben" };
            let signature = git2::Signature::new(
                name,
                "fixture@example.invalid",
                &git2::Time::new((n + 1) * 1000, 0),
            )?;
            let parent = commits
                .last()
                .map(|oid| repo.find_commit(*oid))
                .transpose()?;
            let parents: Vec<_> = parent.iter().collect();
            commits.push(repo.commit(
                Some("HEAD"),
                &signature,
                &signature,
                &format!("Implement preview feature {n}"),
                &tree,
                &parents,
            )?);
        }
        drop(repo);
        let registry = Registry::open(&owned.path().join("registry.redb"))?;
        Ok(Self {
            registry: Some(registry),
            root,
            commits,
            owned,
        })
    }
    async fn query(&self, tip: usize, filter: CardFilter) -> Result<BTreeSet<String>> {
        let cards = metadata_for_line_filtered_cached(
            &self.root,
            self.commits[tip],
            self.registry.as_ref().unwrap(),
            filter,
        )
        .await?;
        Ok(cards.iter().map(|card| card.oid.clone()).collect())
    }
    fn persisted(&mut self) -> Result<BTreeSet<String>> {
        // Inspect only this fixture's closed physical database. Cache keys are
        // observed as persisted; the test does not reproduce their scope hash.
        drop(self.registry.take());
        let path = self.owned.path().join("registry.redb");
        let result = {
            let db = redb::Database::open(&path)?;
            let tx = db.begin_read()?;
            let ids =
                match tx.open_table(TableDefinition::<&str, &[u8]>::new("change_card_cache_v1")) {
                    Ok(rows) => rows
                        .iter()?
                        .map(|row| {
                            let (key, _) = row?;
                            Ok(key.value().rsplit('/').next().unwrap().to_owned())
                        })
                        .collect::<Result<BTreeSet<_>>>()?,
                    Err(redb::TableError::TableDoesNotExist(_)) => BTreeSet::new(),
                    Err(error) => return Err(error.into()),
                };
            ids
        };
        self.registry = Some(Registry::open(&path)?);
        Ok(result)
    }
    fn ids(&self, indexes: &[usize]) -> BTreeSet<String> {
        indexes
            .iter()
            .map(|&n| self.commits[n].to_string())
            .collect()
    }
    fn close(self) -> Result<()> {
        let Self {
            registry, owned, ..
        } = self;
        drop(registry);
        owned.close()?;
        Ok(())
    }
}

#[tokio::test]
async fn cold_author_and_date_scope_persists_only_eligible_commit_metadata() -> Result<()> {
    let mut fixture = Fixture::new()?;
    let selected = fixture
        .query(
            5,
            CardFilter {
                author: Some("Ada".into()),
                date_after: Some(3000),
                date_before: Some(5000),
                ..Default::default()
            },
        )
        .await?;
    ensure!(selected == fixture.ids(&[2]));
    ensure!(
        fixture.persisted()? == fixture.ids(&[2]),
        "Header-excluded commits must not populate cold card metadata"
    );
    fixture.close()
}

#[tokio::test]
async fn no_matching_headers_leave_no_card_cache_and_later_unfiltered_reads_remain_complete()
-> Result<()> {
    let mut fixture = Fixture::new()?;
    ensure!(
        fixture
            .query(
                5,
                CardFilter {
                    author: Some("Nobody".into()),
                    ..Default::default()
                }
            )
            .await?
            .is_empty()
    );
    ensure!(fixture.persisted()?.is_empty());
    ensure!(fixture.query(5, CardFilter::default()).await? == fixture.ids(&[0, 1, 2, 3, 4, 5]));
    ensure!(fixture.persisted()? == fixture.ids(&[0, 1, 2, 3, 4, 5]));
    fixture.close()
}

#[tokio::test]
async fn restart_changed_filters_and_revision_scope_preserve_existing_history_predicates()
-> Result<()> {
    let mut fixture = Fixture::new()?;
    ensure!(fixture.query(2, CardFilter::default()).await? == fixture.ids(&[0, 1, 2]));
    ensure!(fixture.persisted()? == fixture.ids(&[0, 1, 2]));
    ensure!(
        fixture
            .query(
                5,
                CardFilter {
                    author: Some("Ben".into()),
                    ..Default::default()
                }
            )
            .await?
            == fixture.ids(&[1, 3, 5])
    );
    ensure!(fixture.persisted()? == fixture.ids(&[0, 1, 2, 3, 5]));
    ensure!(
        fixture
            .query(
                5,
                CardFilter {
                    author: Some("ben".into()),
                    ..Default::default()
                }
            )
            .await?
            .is_empty()
    );
    ensure!(
        fixture
            .query(
                5,
                CardFilter {
                    file_prefix: Some("Site/".into()),
                    date_after: Some(2000),
                    date_before: Some(4000),
                    ..Default::default()
                }
            )
            .await?
            == fixture.ids(&[1, 2])
    );
    ensure!(
        fixture
            .query(
                5,
                CardFilter {
                    excluded_prefixes: vec!["Site/".into()],
                    ..Default::default()
                }
            )
            .await?
            .is_empty()
    );
    fixture.close()
}
