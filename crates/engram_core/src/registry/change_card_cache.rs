//! Optional derived history metadata, sharing the Registry database/lifetime.
//! Hits never touch insertion order. All writes and bounded FIFO eviction are
//! atomic; unknown future record formats are preserved rather than collected.
use super::*;
use redb::ReadableTableMetadata;

const ROWS: TableDefinition<&str, &[u8]> = TableDefinition::new("change_card_cache_v1");
const ORDER: TableDefinition<u64, &str> = TableDefinition::new("change_card_cache_order_v1");
const STATE: TableDefinition<&str, &[u8]> = TableDefinition::new("change_card_cache_state_v1");
const MAGIC: &[u8; 8] = b"EGCC\0\0\0\x01";
const STATE_MAGIC: &[u8; 8] = b"EGCS\0\0\0\x01";
pub const CHANGE_CARD_CACHE_BATCH: usize = 256;
pub const CHANGE_CARD_CACHE_ROWS: usize = 8192;
pub const CHANGE_CARD_CACHE_BYTES: usize = 32 * 1024 * 1024;
pub const CHANGE_CARD_CACHE_RECORD_BYTES: usize = 64 * 1024;
const MAX_BYTES: usize = CHANGE_CARD_CACHE_BYTES;
const MAX_RECORD: usize = CHANGE_CARD_CACHE_RECORD_BYTES;
const MAX_EVICTIONS: usize = 256;

#[derive(Debug, Default, serde::Serialize)]
pub struct ChangeCardCacheWrite {
    pub stored: usize,
    pub unchanged: usize,
    pub skipped: usize,
    pub evicted: usize,
    /// Logical stored bytes, including keys and record headers; not Redb file size.
    pub charged_bytes: usize,
    pub rows: usize,
}

#[derive(Default)]
struct State {
    next: u64,
    count: usize,
    bytes: usize,
}
impl State {
    fn decode(bytes: &[u8]) -> anyhow::Result<Self> {
        anyhow::ensure!(
            bytes.len() == 32 && &bytes[..8] == STATE_MAGIC,
            "Unknown card-cache state; preserved"
        );
        let number = |offset| u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
        let result = Self {
            next: number(8),
            count: number(16).try_into()?,
            bytes: number(24).try_into()?,
        };
        anyhow::ensure!(
            result.count <= CHANGE_CARD_CACHE_ROWS
                && result.bytes <= MAX_BYTES
                && result.next >= result.count as u64,
            "Invalid card-cache accounting; preserved"
        );
        Ok(result)
    }
    fn encode(&self) -> Vec<u8> {
        [
            STATE_MAGIC.as_slice(),
            &self.next.to_le_bytes(),
            &(self.count as u64).to_le_bytes(),
            &(self.bytes as u64).to_le_bytes(),
        ]
        .concat()
    }
}

fn valid_key(key: &str) -> bool {
    key.split_once('/').is_some_and(|(scope, oid)| {
        scope.len() == 64
            && [40, 64].contains(&oid.len())
            && scope
                .bytes()
                .chain(oid.bytes())
                .all(|b| b.is_ascii_hexdigit())
    })
}
fn charge(key: &str, value_len: usize) -> usize {
    2 * key.len() + value_len + 8
}
fn sequence(value: &[u8]) -> Option<u64> {
    (value.len() >= 16 && &value[..8] == MAGIC)
        .then(|| u64::from_le_bytes(value[8..16].try_into().unwrap()))
}

impl Registry {
    pub fn read_change_card_metadata(
        &self,
        keys: &[String],
    ) -> anyhow::Result<Vec<Option<Vec<u8>>>> {
        anyhow::ensure!(
            keys.len() <= CHANGE_CARD_CACHE_BATCH && keys.iter().all(|key| valid_key(key)),
            "Invalid card-cache lookup"
        );
        let tx = self.db.begin_read()?;
        let rows = match tx.open_table(ROWS) {
            Ok(rows) => rows,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(vec![None; keys.len()]),
            Err(error) => return Err(error.into()),
        };
        let accounting = tx.open_table(STATE)?;
        let state = accounting
            .get("current")?
            .ok_or_else(|| anyhow::anyhow!("Missing card-cache accounting; preserved"))?;
        let state = State::decode(state.value())?;
        anyhow::ensure!(
            rows.len()? == state.count as u64,
            "Inconsistent card-cache accounting; preserved"
        );
        keys.iter()
            .map(|key| -> anyhow::Result<_> {
                let Some(value) = rows.get(key.as_str())? else {
                    return Ok(None);
                };
                let value = value.value();
                if value.len() > MAX_RECORD + 16 || sequence(value).is_none() {
                    return Ok(None);
                }
                Ok(Some(value[16..].to_vec()))
            })
            .collect()
    }

    pub fn store_change_card_metadata(
        &self,
        entries: &[(String, Vec<u8>)],
    ) -> anyhow::Result<ChangeCardCacheWrite> {
        self.store_cards_with_limits(entries, CHANGE_CARD_CACHE_ROWS, MAX_BYTES)
    }

    fn store_cards_with_limits(
        &self,
        entries: &[(String, Vec<u8>)],
        max_rows: usize,
        max_bytes: usize,
    ) -> anyhow::Result<ChangeCardCacheWrite> {
        anyhow::ensure!(
            entries.len() <= CHANGE_CARD_CACHE_BATCH
                && entries.iter().all(|(key, _)| valid_key(key)),
            "Invalid card-cache batch"
        );
        let protected: std::collections::BTreeSet<_> =
            entries.iter().map(|(key, _)| key.as_str()).collect();
        anyhow::ensure!(protected.len() == entries.len(), "Duplicate card-cache key");
        let mut result = ChangeCardCacheWrite::default();
        if entries.is_empty() {
            return Ok(result);
        }
        let tx = self.db.begin_write()?;
        {
            let mut rows = tx.open_table(ROWS)?;
            let mut order = tx.open_table(ORDER)?;
            let mut state_table = tx.open_table(STATE)?;
            let mut state = match state_table.get("current")? {
                Some(raw) => State::decode(raw.value())?,
                None => {
                    anyhow::ensure!(
                        rows.is_empty()? && order.is_empty()?,
                        "Missing card-cache accounting; preserved"
                    );
                    State::default()
                }
            };
            anyhow::ensure!(
                rows.len()? == state.count as u64 && order.len()? == state.count as u64,
                "Inconsistent card-cache accounting; preserved"
            );
            // Read at most this many eviction candidates once per transaction.
            let mut candidates = Vec::new();
            for row in order.iter()?.take(MAX_EVICTIONS) {
                let (seq, key) = row?;
                let key = key.value();
                if protected.contains(key) {
                    continue;
                }
                let Some(value) = rows.get(key)? else {
                    continue;
                };
                if sequence(value.value()) == Some(seq.value()) {
                    candidates.push((
                        seq.value(),
                        key.to_owned(),
                        charge(key, value.value().len()),
                    ));
                }
            }
            let mut cursor = 0;
            for (key, payload) in entries {
                if payload.len() > MAX_RECORD {
                    result.skipped += 1;
                    continue;
                }
                let prior = rows.get(key.as_str())?.map(|raw| {
                    let raw = raw.value();
                    (
                        sequence(raw),
                        raw.len(),
                        raw.len() >= 16 && &raw[16..] == payload.as_slice(),
                    )
                });
                if prior.as_ref().is_some_and(|(seq, _, _)| seq.is_none()) {
                    result.skipped += 1;
                    continue;
                }
                if let Some((Some(seq), _, _)) = &prior {
                    anyhow::ensure!(
                        order
                            .get(*seq)?
                            .is_some_and(|value| value.value() == key.as_str()),
                        "Inconsistent card-cache insertion order; preserved"
                    );
                }
                if prior.as_ref().is_some_and(|(_, _, same)| *same) {
                    result.unchanged += 1;
                    continue;
                }
                let old_charge = prior.as_ref().map_or(0, |(_, len, _)| charge(key, *len));
                let new_charge = charge(key, 16 + payload.len());
                let mut projected_count = state.count + usize::from(prior.is_none());
                let mut projected_bytes = state.bytes.checked_sub(old_charge).ok_or_else(|| {
                    anyhow::anyhow!("Invalid card-cache byte accounting; preserved")
                })? + new_charge;
                let mut end = cursor;
                while (projected_count > max_rows || projected_bytes > max_bytes)
                    && end < candidates.len()
                {
                    let (_, candidate, bytes) = &candidates[end];
                    if candidate != key && rows.get(candidate.as_str())?.is_some() {
                        projected_count -= 1;
                        projected_bytes = projected_bytes.checked_sub(*bytes).ok_or_else(|| {
                            anyhow::anyhow!("Invalid card-cache eviction accounting; preserved")
                        })?;
                    }
                    end += 1;
                }
                if projected_count > max_rows || projected_bytes > max_bytes {
                    result.skipped += 1;
                    continue;
                }
                for (seq, candidate, _) in &candidates[cursor..end] {
                    if candidate != key && rows.remove(candidate.as_str())?.is_some() {
                        order.remove(*seq)?;
                        result.evicted += 1;
                    }
                }
                cursor = end;
                let seq = if let Some((Some(seq), _, _)) = &prior {
                    *seq
                } else {
                    state.next = state.next.checked_add(1).ok_or_else(|| {
                        anyhow::anyhow!("Card-cache insertion sequence exhausted")
                    })?;
                    state.next
                };
                let raw = [MAGIC.as_slice(), &seq.to_le_bytes(), payload.as_slice()].concat();
                rows.insert(key.as_str(), raw.as_slice())?;
                order.insert(seq, key.as_str())?;
                state.count = projected_count;
                state.bytes = projected_bytes;
                result.stored += 1;
            }
            result.rows = state.count;
            result.charged_bytes = state.bytes;
            if result.stored != 0 || result.evicted != 0 {
                state_table.insert("current", state.encode().as_slice())?;
            }
        }
        if result.stored != 0 || result.evicted != 0 {
            tx.commit()?;
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(n: usize) -> String {
        format!("{}/{}", "a".repeat(64), format!("{n:040x}"))
    }
    #[test]
    fn card_retention_reopens_bounded_and_hits_do_not_touch_fifo() -> anyhow::Result<()> {
        let owned = tempfile::tempdir()?;
        let path = owned.path().join("registry.redb");
        let registry = Registry::open(&path)?;
        for n in 0..300 {
            let result =
                registry.store_cards_with_limits(&[(key(n), vec![n as u8; 64])], 4, 2048)?;
            assert!(result.rows <= 4 && result.charged_bytes <= 2048);
        }
        let before = {
            let tx = registry.db.begin_read()?;
            let value = tx
                .open_table(STATE)?
                .get("current")?
                .unwrap()
                .value()
                .to_vec();
            value
        };
        assert!(registry.read_change_card_metadata(&[key(0)])?[0].is_none());
        assert_eq!(
            registry.read_change_card_metadata(&[key(299)])?[0],
            Some(vec![43; 64])
        );
        assert_eq!(
            registry
                .store_cards_with_limits(&[(key(299), vec![43; 64])], 4, 2048)?
                .unchanged,
            1
        );
        let after = {
            let tx = registry.db.begin_read()?;
            let value = tx
                .open_table(STATE)?
                .get("current")?
                .unwrap()
                .value()
                .to_vec();
            value
        };
        assert_eq!(
            before, after,
            "Hits must not refresh insertion order or persisted state"
        );
        drop(registry);
        let registry = Registry::open(&path)?;
        assert_eq!(
            registry.read_change_card_metadata(&[key(299)])?[0],
            Some(vec![43; 64])
        );
        drop(registry);
        owned.close()?;
        Ok(())
    }

    #[test]
    fn a_batch_update_cannot_be_evicted_using_an_earlier_size() -> anyhow::Result<()> {
        let owned = tempfile::tempdir()?;
        let registry = Registry::open(&owned.path().join("registry.redb"))?;
        registry.store_cards_with_limits(
            &[(key(0), vec![0; 64]), (key(1), vec![1; 64])],
            2,
            1024,
        )?;
        let result = registry.store_cards_with_limits(
            &[(key(0), vec![2; 400]), (key(2), vec![])],
            2,
            1024,
        )?;
        assert!(result.charged_bytes <= 1024 && result.rows <= 2);
        let rows = registry.read_change_card_metadata(&[key(0), key(1), key(2)])?;
        assert_eq!(rows, vec![Some(vec![2; 400]), None, Some(vec![])]);
        drop(registry);
        owned.close()?;
        Ok(())
    }

    #[test]
    fn skipped_admission_does_not_consume_eviction_candidates() -> anyhow::Result<()> {
        let owned = tempfile::tempdir()?;
        let registry = Registry::open(&owned.path().join("registry.redb"))?;
        registry.store_cards_with_limits(
            &[(key(0), vec![0; 64]), (key(1), vec![1; 64])],
            2,
            1024,
        )?;
        let result = registry.store_cards_with_limits(
            &[(key(2), vec![2; 2000]), (key(3), vec![3; 64])],
            2,
            1024,
        )?;
        assert_eq!(result.skipped, 1);
        assert_eq!(result.stored, 1);
        assert_eq!(result.evicted, 1);
        assert_eq!(
            registry.read_change_card_metadata(&[key(0), key(1), key(2), key(3)])?,
            vec![None, Some(vec![1; 64]), None, Some(vec![3; 64])]
        );
        assert!(result.charged_bytes <= 1024 && result.rows <= 2);
        drop(registry);
        owned.close()?;
        Ok(())
    }

    #[test]
    fn an_unknown_record_is_neither_replaced_nor_evicted() -> anyhow::Result<()> {
        let owned = tempfile::tempdir()?;
        let registry = Registry::open(&owned.path().join("registry.redb"))?;
        registry.store_cards_with_limits(&[(key(0), b"future payload".to_vec())], 1, 1024)?;
        let future = {
            let tx = registry.db.begin_write()?;
            let mut rows = tx.open_table(ROWS)?;
            let mut value = rows.get(key(0).as_str())?.unwrap().value().to_vec();
            value[7] = 2;
            rows.insert(key(0).as_str(), value.as_slice())?;
            drop(rows);
            tx.commit()?;
            value
        };
        assert!(registry.read_change_card_metadata(&[key(0)])?[0].is_none());
        assert_eq!(
            registry
                .store_cards_with_limits(&[(key(0), b"overwrite".to_vec())], 1, 1024)?
                .skipped,
            1
        );
        assert_eq!(
            registry
                .store_cards_with_limits(&[(key(1), b"evict".to_vec())], 1, 1024)?
                .skipped,
            1
        );
        let actual = {
            let tx = registry.db.begin_read()?;
            let value = tx
                .open_table(ROWS)?
                .get(key(0).as_str())?
                .unwrap()
                .value()
                .to_vec();
            value
        };
        assert_eq!(actual, future);
        drop(registry);
        owned.close()?;
        Ok(())
    }
}
