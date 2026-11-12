//! Shared bounded map with insertion-order (oldest-first) eviction.
//!
//! Rule 17 (V5 Wave 2): bounded state NEVER wholesale-clears. The
//! pre-M-05 `HashMap` + `clear()` pattern (audit I-06) reset every
//! tracked user/tenant at once — a single flood could have wiped all
//! budgets. `BoundedMap` evicts exactly ONE oldest entry per insertion
//! beyond the cap, and is the single eviction helper shared by the
//! per-tenant RPC governor (`grpc::interceptors::rate_limit`) and the
//! per-username login throttle (`security::throttle`).

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;

pub(crate) struct BoundedMap<K, V> {
    max: usize,
    map: HashMap<K, V>,
    /// Insertion order — the eviction queue (oldest first).
    order: VecDeque<K>,
}

impl<K: Eq + Hash + Clone, V> BoundedMap<K, V> {
    pub fn new(max: usize) -> Self {
        Self {
            max: max.max(1),
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    /// Returns the value for `key`, inserting via `make` when absent.
    /// Insertion beyond the cap evicts the OLDEST entry.
    pub fn get_or_insert<F: FnOnce() -> V>(&mut self, key: K, make: F) -> &mut V {
        if !self.map.contains_key(&key) && self.map.len() >= self.max {
            if let Some(oldest) = self.order.pop_front() {
                self.map.remove(&oldest);
            }
        }
        if !self.map.contains_key(&key) {
            self.order.push_back(key.clone());
            self.map.insert(key.clone(), make());
        }
        self.map
            .get_mut(&key)
            .expect("key was just inserted or already present")
    }

    /// Reads a value without inserting.
    pub fn get(&self, key: &K) -> Option<&V> {
        self.map.get(key)
    }

    /// Reads a value mutably without inserting (None when absent).
    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        self.map.get_mut(key)
    }

    /// Removes a key (backoff reset).
    pub fn remove(&mut self, key: &K) {
        if self.map.remove(key).is_some() {
            self.order.retain(|k| k != key);
        }
    }

    /// Tracked-entry count (test/gate diagnostics).
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.map.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oldest_evicted_one_per_insertion() {
        let mut m: BoundedMap<u32, u32> = BoundedMap::new(3);
        for i in 0..3 {
            m.get_or_insert(i, || 0);
        }
        assert_eq!(m.len(), 3);
        m.get_or_insert(3, || 0);
        assert_eq!(m.len(), 3, "cap enforced");
        assert!(m.get(&0).is_none(), "oldest evicted");
        assert!(m.get(&1).is_some(), "second-oldest survives");
        assert!(m.get(&3).is_some(), "newest present");
    }

    #[test]
    fn remove_and_reinsert_moves_to_back() {
        let mut m: BoundedMap<u32, u32> = BoundedMap::new(3);
        for i in 0..3 {
            m.get_or_insert(i, || 0);
        }
        m.remove(&0);
        m.get_or_insert(0, || 9);
        // 1 is now the oldest.
        m.get_or_insert(4, || 0);
        assert!(m.get(&1).is_none());
        assert_eq!(m.get(&0), Some(&9));
    }
}
