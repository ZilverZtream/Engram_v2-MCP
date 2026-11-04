//! One derived current ranking record; obsolete generations are not history.
use super::*;

const CURRENT: &str = "current_v1";
const LEGACY_CLEANUP_BATCH: usize = 32;

const LEGACY_SCAN_LIMIT: usize = 128;
const MAGIC: &[u8; 8] = b"EGPR\0\0\0\x01";

fn stored_generation(bytes: &[u8]) -> anyhow::Result<u64> {
    anyhow::ensure!(
        bytes.len() >= 16 && &bytes[..8] == MAGIC,
        "Unknown or truncated derived centrality cache format"
    );
    Ok(u64::from_le_bytes(bytes[8..16].try_into()?))
}

impl GraphStore {
    pub fn get_cached_centrality(
        &self,
        project_id: &str,
        generation: u64,
    ) -> anyhow::Result<Option<HashMap<String, f32>>> {
        let rtx = self.db.begin_read()?;
        let ct = rtx.open_table(CENTRALITY)?;
        let key = format!("{project_id}\0{CURRENT}");
        let Some(value) = ct.get(key.as_str())? else {
            return Ok(None);
        };
        if stored_generation(value.value())? != generation {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&value.value()[16..])?))
    }

    pub fn set_cached_centrality(
        &self,
        project_id: &str,
        generation: u64,
        metrics: &HashMap<String, f32>,
    ) -> anyhow::Result<()> {
        let wtx = self.db.begin_write()?;
        let removed;
        {
            let mut ct = wtx.open_table(CENTRALITY)?;
            let prefix = format!("{project_id}\0");
            let key = format!("{prefix}{CURRENT}");
            let existing = ct
                .get(key.as_str())?
                .map(|value| stored_generation(value.value()))
                .transpose()?;
            // Redb's write transaction serializes this comparison and write.
            // An older worker cannot replace a newer result, even if its
            // in-memory caller later discards that worker's return value.
            if existing.is_none_or(|previous| previous <= generation) {
                let mut bytes = Vec::with_capacity(16);
                bytes.extend_from_slice(MAGIC);
                bytes.extend_from_slice(&generation.to_le_bytes());
                serde_json::to_writer(&mut bytes, metrics)?;
                ct.insert(key.as_str(), bytes.as_slice())?;
            }
            // Drain existing derived cache bloat without an unbounded key list
            // or deserializing its maps. This does not shrink the physical DB.
            let mut legacy = Vec::with_capacity(LEGACY_CLEANUP_BATCH);
            for row in ct.range(prefix.as_str()..)?.take(LEGACY_SCAN_LIMIT) {
                let (old_key, _) = row?;
                let old_key = old_key.value();
                if !old_key.starts_with(&prefix) {
                    break;
                }
                let suffix = &old_key[prefix.len()..];
                if suffix
                    .parse::<u64>()
                    .ok()
                    .is_some_and(|generation| generation.to_string() == suffix)
                {
                    legacy.push(old_key.to_owned());
                }
                if legacy.len() == LEGACY_CLEANUP_BATCH {
                    break;
                }
            }
            removed = legacy.len();
            for old_key in legacy {
                ct.remove(old_key.as_str())?;
            }
        }
        wtx.commit()?;
        if removed > 0 {
            tracing::debug!(
                project_id,
                removed,
                batch_limit = LEGACY_CLEANUP_BATCH,
                "Removed bounded batch of obsolete derived centrality cache records"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
