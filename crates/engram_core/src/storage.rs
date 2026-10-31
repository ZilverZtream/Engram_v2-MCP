//! Explicit page-cache limits for persistent stores. redb's default is 1 GiB
//! per database, which multiplies across projects and bypasses allocation guards.

use std::path::Path;

#[derive(Clone, Copy)]
pub enum DatabaseCache {
    Metadata,
    Documents,
    Graph,
    Embeddings,
}

pub fn open_database(path: &Path, cache: DatabaseCache) -> anyhow::Result<redb::Database> {
    let mebibytes = match cache {
        DatabaseCache::Metadata => 16,
        DatabaseCache::Documents => 32,
        DatabaseCache::Graph | DatabaseCache::Embeddings => 128,
    };
    Ok(redb::Database::builder()
        .set_cache_size(mebibytes * 1024 * 1024)
        .create(path)?)
}
