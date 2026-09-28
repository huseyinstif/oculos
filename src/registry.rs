//! Session registry mapping `oculos_id` → native element handle.
//!
//! IDs are derived from a stable, platform-specific identity of the element
//! (UIA RuntimeId, AT-SPI bus name + object path, AXUIElement hash), so finding
//! the same element twice yields the same `oculos_id` and repeated polling
//! does not grow the registry. Entries that have not been used for a while are
//! evicted, and the total size is capped.

use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use dashmap::DashMap;

/// Maximum number of live entries before the least recently used are evicted.
pub const DEFAULT_CAPACITY: usize = 50_000;
/// Entries untouched for this long are dropped.
pub const DEFAULT_TTL: Duration = Duration::from_secs(30 * 60);
/// Run a full maintenance sweep every N inserts even if under capacity.
const SWEEP_EVERY: u64 = 4096;

struct Slot<T> {
    value: T,
    touched: Instant,
}

pub struct ElementRegistry<T> {
    map: DashMap<String, Slot<T>>,
    capacity: usize,
    ttl: Duration,
    inserts: AtomicU64,
    sweeping: AtomicBool,
}

impl<T: Clone> Default for ElementRegistry<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Clone> ElementRegistry<T> {
    pub fn new() -> Self {
        Self::with_limits(DEFAULT_CAPACITY, DEFAULT_TTL)
    }

    pub fn with_limits(capacity: usize, ttl: Duration) -> Self {
        Self {
            map: DashMap::new(),
            capacity: capacity.max(1),
            ttl,
            inserts: AtomicU64::new(0),
            sweeping: AtomicBool::new(false),
        }
    }

    /// Insert or refresh an entry.
    pub fn insert(&self, id: String, value: T) {
        self.map.insert(
            id,
            Slot {
                value,
                touched: Instant::now(),
            },
        );
        let n = self.inserts.fetch_add(1, Ordering::Relaxed) + 1;
        if self.map.len() > self.capacity || n.is_multiple_of(SWEEP_EVERY) {
            self.sweep();
        }
    }

    /// Look up an entry, refreshing its last-used time.
    pub fn get(&self, id: &str) -> Option<T> {
        let expired = {
            let mut slot = self.map.get_mut(id)?;
            if slot.touched.elapsed() > self.ttl {
                true
            } else {
                slot.touched = Instant::now();
                return Some(slot.value.clone());
            }
        };
        if expired {
            self.map.remove(id);
        }
        None
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Drop expired entries; if still over capacity, drop the least recently
    /// used ones until the registry is at 75% of capacity.
    fn sweep(&self) {
        if self.sweeping.swap(true, Ordering::Acquire) {
            return; // another thread is already sweeping
        }

        let now = Instant::now();
        let mut ages: Vec<(String, Instant)> = self
            .map
            .iter()
            .map(|e| (e.key().clone(), e.value().touched))
            .collect();

        let mut remaining = ages.len();
        ages.retain(|(id, touched)| {
            if now.duration_since(*touched) > self.ttl {
                self.map.remove(id);
                remaining -= 1;
                false
            } else {
                true
            }
        });

        if remaining > self.capacity {
            let target = self.capacity * 3 / 4;
            ages.sort_by_key(|(_, touched)| *touched);
            for (id, _) in ages.iter().take(remaining - target) {
                self.map.remove(id);
            }
        }

        self.sweeping.store(false, Ordering::Release);
    }
}

/// Derive a short, deterministic `oculos_id` from a stable element identity.
pub fn stable_id(key: impl Hash) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    format!("{:016x}", h.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_key_same_id() {
        assert_eq!(stable_id((":1.42", "/a/b")), stable_id((":1.42", "/a/b")));
        assert_ne!(stable_id((":1.42", "/a/b")), stable_id((":1.42", "/a/c")));
        assert_eq!(stable_id(1u32).len(), 16);
    }

    #[test]
    fn reinsert_does_not_grow() {
        let r = ElementRegistry::new();
        for _ in 0..10 {
            r.insert("x".into(), 1);
        }
        assert_eq!(r.len(), 1);
        assert_eq!(r.get("x"), Some(1));
        assert_eq!(r.get("missing"), None);
    }

    #[test]
    fn capacity_evicts_least_recently_used() {
        let r = ElementRegistry::with_limits(100, DEFAULT_TTL);
        for i in 0..100 {
            r.insert(format!("e{i}"), i);
            std::thread::sleep(Duration::from_micros(50));
        }
        // Touch e0 so it becomes the most recently used.
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(r.get("e0"), Some(0));
        r.insert("overflow".into(), 999);
        assert!(r.len() <= 100);
        assert_eq!(r.get("e0"), Some(0), "recently used entry must survive");
        assert_eq!(r.get("overflow"), Some(999));
        assert_eq!(r.get("e1"), None, "oldest entry must be evicted");
    }

    #[test]
    fn expired_entries_are_dropped() {
        let r = ElementRegistry::with_limits(100, Duration::from_millis(1));
        r.insert("x".into(), 1);
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(r.get("x"), None);
        assert_eq!(r.len(), 0);
    }
}
