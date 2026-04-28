use crate::catalog::SegmentId;
use std::collections::HashMap;

/// Maps segment name strings to their assigned `SegmentId` integers.
///
/// Built during catalog load (`&mut self` inserts before Arc wrapping).
/// Merged append-only on background refresh via `merge(&mut self)`.
/// IDs are never evicted so in-flight requests always resolve correctly.
#[derive(Debug, Default, Clone)]
pub struct SegmentRegistry {
    inner: HashMap<String, SegmentId>,
}

impl SegmentRegistry {
    pub fn insert(&mut self, name: String, id: SegmentId) {
        self.inner.insert(name, id);
    }

    pub fn resolve(&self, names: &[String]) -> Vec<SegmentId> {
        names
            .iter()
            .filter_map(|n| self.inner.get(n).copied())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Merges entries from another registry without evicting existing IDs.
    /// Called by the background refresh task on the shared `Arc<SegmentRegistry>`.
    ///
    /// Requires `&mut self` — the caller holds exclusive access through
    /// `Arc::get_mut` or by constructing a new Arc after merging.
    pub fn merge(&mut self, other: SegmentRegistry) {
        for (k, v) in other.inner {
            self.inner.entry(k).or_insert(v);
        }
    }
}
