use crate::catalog::SegmentId;
use moka::future::Cache;
use std::{sync::Arc, time::Duration};

/// Moka W-TinyLFU async cache for user → segment IDs.
///
/// Wraps `moka::future::Cache` so callers get a clean `get_or_fetch` API
/// without exposing moka types to the pipeline stage.
#[derive(Clone)]
pub struct SegmentCache {
    inner: Cache<Arc<str>, Arc<Vec<SegmentId>>>,
}

impl SegmentCache {
    pub fn new(capacity: u64, ttl_secs: u64) -> Self {
        Self {
            inner: Cache::builder()
                .max_capacity(capacity)
                .time_to_live(Duration::from_secs(ttl_secs))
                .build(),
        }
    }

    /// Returns segments from cache, collapsing concurrent misses for the same
    /// user into a single Redis fetch via moka's `try_get_with`.
    pub async fn get_or_fetch<F, Fut>(
        &self,
        user_id: &str,
        fetch: F,
    ) -> anyhow::Result<Arc<Vec<SegmentId>>>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<Vec<SegmentId>>>,
    {
        let key: Arc<str> = Arc::from(user_id);
        self.inner
            .try_get_with(key, async move { fetch().await.map(Arc::new) })
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    }
}
