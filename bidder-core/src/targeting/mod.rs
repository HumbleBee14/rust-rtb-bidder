use crate::catalog::SegmentId;
use std::collections::HashMap;

/// Maps segment name strings to their assigned `SegmentId` integers.
///
/// Built during catalog load (`&mut self` inserts before Arc wrapping).
/// On background refresh a fresh registry is built from Postgres and swapped
/// atomically via `ArcSwap` — no merge required since Postgres is the source of truth.
#[derive(Debug, Default)]
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_known_and_unknown() {
        let mut reg = SegmentRegistry::default();
        reg.insert("sports-fans".into(), 1);
        reg.insert("gamers".into(), 2);

        let ids = reg.resolve(&[
            "sports-fans".to_string(),
            "unknown".to_string(),
            "gamers".to_string(),
        ]);
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn resolve_empty_input() {
        let mut reg = SegmentRegistry::default();
        reg.insert("sports-fans".into(), 1);
        assert!(reg.resolve(&[]).is_empty());
    }

    #[test]
    fn len_and_is_empty() {
        let mut reg = SegmentRegistry::default();
        assert!(reg.is_empty());
        reg.insert("a".into(), 1);
        assert_eq!(reg.len(), 1);
        assert!(!reg.is_empty());
    }
}
