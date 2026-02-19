use git2::Repository;
use std::path::Path;

#[derive(Clone)]
pub struct GitCollector;

impl GitCollector {
    pub fn open_repo(path: &Path) -> anyhow::Result<Repository> {
        Ok(Repository::discover(path)?)
    }

    pub fn count_commits(repo: &Repository) -> anyhow::Result<usize> {
        let mut revwalk = repo.revwalk()?;
        revwalk.push_head()?;
        Ok(revwalk.count())
    }
}
