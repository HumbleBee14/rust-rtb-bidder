use crate::{
    cache::SegmentCache, catalog::SharedCatalog, model::context::BidContext,
    pipeline::stage::Stage, repository::UserSegmentRepository,
};
use std::sync::Arc;

/// Enriches the request context with user segment IDs from the cache/Redis
/// and attaches the current catalog snapshot.
///
/// Must run before CandidateRetrievalStage so `ctx.segment_ids` and
/// `ctx.catalog` are populated. A missing `user.id` is not fatal — the
/// pipeline continues with an empty segment list (targeting assumes no
/// user segments, which is valid).
pub struct UserEnrichmentStage {
    pub catalog: SharedCatalog,
    pub segment_cache: SegmentCache,
    pub segment_repo: Arc<dyn UserSegmentRepository>,
}

impl Stage for UserEnrichmentStage {
    fn name(&self) -> &'static str {
        "user_enrichment"
    }

    #[allow(clippy::manual_async_fn)]
    fn execute<'a>(
        &'a self,
        ctx: &'a mut BidContext,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send + 'a {
        async move {
            // Attach catalog snapshot — cheap Arc clone.
            ctx.catalog = Some(self.catalog.load_full());

            // Resolve user segments if a user.id is present.
            if let Some(user_id) = ctx.request.user.as_ref().and_then(|u| u.id.as_deref()) {
                let repo = &*self.segment_repo;
                match self
                    .segment_cache
                    .get_or_fetch(user_id, || repo.segments_for(user_id))
                    .await
                {
                    Ok(segments) => {
                        ctx.segment_ids = (*segments).clone();
                    }
                    Err(e) => {
                        // Non-fatal: proceed with empty segments, emit a counter.
                        metrics::counter!("bidder.user_enrichment.segment_fetch_error")
                            .increment(1);
                        tracing::warn!(error = %e, user_id, "segment fetch failed, continuing without segments");
                    }
                }
            }

            Ok(())
        }
    }
}
