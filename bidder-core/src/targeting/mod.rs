use crate::catalog::SegmentId;
use std::{collections::HashMap, sync::RwLock};

/// Thread-safe registry mapping segment name → SegmentId.
///
/// Built at catalog load time; merged (not replaced) on background refresh
/// so IDs already handed to in-flight requests remain valid.
#[derive(Debug, Default)]
pub struct SegmentRegistry {
    inner: RwLock<HashMap<String, SegmentId>>,
}

impl SegmentRegistry {
    pub fn insert(&mut self, name: String, id: SegmentId) {
        self.inner.get_mut().unwrap().insert(name, id);
    }

    pub fn resolve(&self, names: &[String]) -> Vec<SegmentId> {
        let map = self.inner.read().unwrap();
        names.iter().filter_map(|n| map.get(n).copied()).collect()
    }

    pub fn len(&self) -> usize {
        self.inner.read().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Merges entries from another registry into this one without evicting
    /// existing IDs. Called by the background refresh task.
    pub fn merge(&self, other: SegmentRegistry) {
        let other_map = other.inner.into_inner().unwrap();
        let mut map = self.inner.write().unwrap();
        for (k, v) in other_map {
            map.entry(k).or_insert(v);
        }
    }
}
