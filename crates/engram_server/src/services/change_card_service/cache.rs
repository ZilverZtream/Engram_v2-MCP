//! Best-effort bounded retention. A request owns its snapshot independently of
//! cache eviction or a different embedding profile completing concurrently.
use super::ChangeCard;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

pub(super) type Cards = HashMap<String, Arc<ChangeCard>>;

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Key {
    pub repository: PathBuf,
    pub profile: String,
}

struct Entry {
    key: Key,
    cards: Cards,
    estimated_bytes: usize,
}

pub(super) struct CardCache {
    entries: VecDeque<Entry>,
    estimated_bytes: usize,
    max_partitions: usize,
    max_bytes: usize,
}

impl CardCache {
    pub fn new(max_partitions: usize, max_bytes: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            estimated_bytes: 0,
            max_partitions,
            max_bytes,
        }
    }

    pub fn snapshot(&mut self, key: &Key) -> Cards {
        let Some(position) = self.entries.iter().position(|entry| &entry.key == key) else {
            return Cards::new();
        };
        let entry = self.entries.remove(position).expect("position checked");
        let cards = entry.cards.clone();
        self.entries.push_back(entry);
        cards
    }

    pub fn retained_bytes(&self, key: &Key) -> usize {
        self.entries.iter().find(|entry| &entry.key == key)
            .map_or(0, |entry| entry.estimated_bytes)
    }

    fn charge(key: &Key, cards: &Cards) -> usize {
        let mut bytes = 512usize
            .saturating_add(
                key.repository
                    .as_os_str()
                    .as_encoded_bytes()
                    .len()
                    .saturating_mul(2),
            )
            .saturating_add(key.profile.capacity())
            .saturating_add(
                cards
                    .capacity()
                    .saturating_mul(std::mem::size_of::<(String, Arc<ChangeCard>)>() + 32),
            );
        for (oid, card) in cards {
            let heap = oid
                .capacity()
                .saturating_add(
                    std::mem::size_of::<ChangeCard>() + 2 * std::mem::size_of::<usize>(),
                )
                .saturating_add(card.oid.capacity())
                .saturating_add(card.title.capacity())
                .saturating_add(card.description.capacity())
                .saturating_add(card.author.capacity())
                .saturating_add(
                    card.files
                        .capacity()
                        .saturating_mul(std::mem::size_of::<String>()),
                )
                .saturating_add(
                    card.files
                        .iter()
                        .map(String::capacity)
                        .fold(0usize, usize::saturating_add),
                )
                .saturating_add(
                    card.vector
                        .capacity()
                        .saturating_mul(std::mem::size_of::<f32>()),
                );
            bytes = bytes.saturating_add(heap);
        }
        bytes
    }

    pub fn retain(&mut self, key: Key, cards: Cards) {
        let estimated_bytes = Self::charge(&key, &cards);
        // Oversized requests stay valid, but cannot displace a useful smaller
        // entry with a partial or unbounded retained partition.
        if self.max_partitions == 0 || estimated_bytes > self.max_bytes {
            return;
        }
        if let Some(position) = self.entries.iter().position(|entry| entry.key == key) {
            let old = self.entries.remove(position).expect("position checked");
            self.estimated_bytes -= old.estimated_bytes;
        }
        while self.entries.len() >= self.max_partitions
            || self.estimated_bytes.saturating_add(estimated_bytes) > self.max_bytes
        {
            let Some(old) = self.entries.pop_front() else {
                break;
            };
            self.estimated_bytes -= old.estimated_bytes;
        }
        self.estimated_bytes += estimated_bytes;
        self.entries.push_back(Entry {
            key,
            cards,
            estimated_bytes,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(profile: &str) -> Key {
        Key {
            repository: PathBuf::from("fixture-repository"),
            profile: profile.into(),
        }
    }

    fn cards() -> Cards {
        let card = Arc::new(ChangeCard {
            oid: "commit".into(),
            title: "Scoped history".into(),
            description: "Evidence".into(),
            author: "Fixture".into(),
            timestamp: 1,
            files: vec!["area/a.rs".into()],
            file_count: 1,
            vector: vec![1.0, 0.0],
        });
        HashMap::from([(card.oid.clone(), card)])
    }

    #[test]
    fn cache_pressure_cannot_change_a_requests_retained_history_or_vectors() {
        let mut cache = CardCache::new(2, 16 * 1024);
        cache.retain(key("A"), cards());
        let in_flight = cache.snapshot(&key("A"));
        cache.retain(key("B"), cards());
        cache.retain(key("C"), cards());
        assert!(cache.snapshot(&key("A")).is_empty());
        assert_eq!(in_flight["commit"].vector, vec![1.0, 0.0]);
        assert_eq!(in_flight["commit"].files, vec!["area/a.rs"]);
        assert!(!cache.snapshot(&key("B")).is_empty());
        assert!(!cache.snapshot(&key("C")).is_empty());
    }

    #[test]
    fn metadata_and_vector_retention_obeys_byte_limit_and_keeps_oversized_results_usable() {
        let small = cards();
        let limit = CardCache::charge(&key("A"), &small);
        let mut cache = CardCache::new(8, limit);
        cache.retain(key("A"), small.clone());
        let mut large = small.clone();
        Arc::make_mut(large.get_mut("commit").unwrap()).description = "large evidence".repeat(1024);
        cache.retain(key("A"), large.clone());
        assert_eq!(cache.snapshot(&key("A"))["commit"].description, "Evidence");
        assert!(large["commit"].description.len() > limit);
        cache.retain(key("B"), small);
        assert!(cache.snapshot(&key("A")).is_empty());
        assert!(!cache.snapshot(&key("B")).is_empty());
    }
}
